use anyhow::Result;
use edit_prediction::cursor_excerpt;
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger,
};
use futures::AsyncReadExt;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::HttpClient;
use icons::IconName;
use language::{
    Anchor, Buffer, BufferSnapshot, EditPreview, language_settings::all_language_settings,
};
use language_model::{ApiKeyState, AuthenticateError, EnvVar, env_var};
use serde::{Deserialize, Serialize};

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};
use text::ToOffset;

pub const CODESTRAL_API_URL: &str = "https://codestral.mistral.ai";

static CODESTRAL_API_KEY_ENV_VAR: std::sync::LazyLock<EnvVar> = env_var!("CODESTRAL_API_KEY");

struct GlobalCodestralApiKey(Entity<ApiKeyState>);

impl Global for GlobalCodestralApiKey {}

pub fn codestral_api_key_state(cx: &mut App) -> Entity<ApiKeyState> {
    if let Some(global) = cx.try_global::<GlobalCodestralApiKey>() {
        return global.0.clone();
    }
    let entity =
        cx.new(|cx| ApiKeyState::new(codestral_api_url(cx), CODESTRAL_API_KEY_ENV_VAR.clone()));
    cx.set_global(GlobalCodestralApiKey(entity.clone()));
    entity
}

pub fn codestral_api_key(cx: &App) -> Option<Arc<str>> {
    let url = codestral_api_url(cx);
    cx.try_global::<GlobalCodestralApiKey>()?
        .0
        .read(cx)
        .key(&url)
}

pub fn load_codestral_api_key(cx: &mut App) -> Task<Result<(), AuthenticateError>> {
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = codestral_api_url(cx);
    codestral_api_key_state(cx).update(cx, |key_state, cx| {
        key_state.load_if_needed(api_url, |s| s, credentials_provider, cx)
    })
}

pub fn codestral_api_url(cx: &App) -> SharedString {
    all_language_settings(None, cx)
        .edit_predictions
        .codestral
        .api_url
        .clone()
        .unwrap_or_else(|| CODESTRAL_API_URL.to_string())
        .into()
}

/// Represents a completion that has been received and processed from Codestral.
/// This struct maintains the state needed to interpolate the completion as the user types.
#[derive(Clone)]
struct CurrentCompletion {
    /// The buffer snapshot at the time the completion was generated.
    /// Used to detect changes and interpolate edits.
    snapshot: BufferSnapshot,
    /// The edits that should be applied to transform the original text into the predicted text.
    /// Each edit is a range in the buffer and the text to replace it with.
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    /// Preview of how the buffer will look after applying the edits.
    edit_preview: EditPreview,
}

impl CurrentCompletion {
    /// Attempts to adjust the edits based on changes made to the buffer since the completion was generated.
    /// Returns None if no predicted edits remain or the user's edits conflict with the predicted edits.
    fn interpolate(&self, new_snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        edit_prediction_types::interpolate_edits(&self.snapshot, new_snapshot, &self.edits)
            .filter(|edits| !edits.is_empty())
    }
}

pub struct CodestralEditPredictionDelegate {
    http_client: Arc<dyn HttpClient>,
    pending_request: Option<Task<Result<()>>>,
    current_completion: Option<CurrentCompletion>,
}

impl CodestralEditPredictionDelegate {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            http_client,
            pending_request: None,
            current_completion: None,
        }
    }

    pub fn ensure_api_key_loaded(cx: &mut App) {
        load_codestral_api_key(cx).detach();
    }

    /// Uses Codestral's Fill-in-the-Middle API for code completion.
    async fn fetch_completion(
        http_client: Arc<dyn HttpClient>,
        api_key: &str,
        prompt: String,
        suffix: String,
        model: String,
        max_tokens: Option<u32>,
        api_url: String,
    ) -> Result<String> {
        let start_time = Instant::now();

        log::debug!(
            "Codestral: Requesting completion (model: {}, max_tokens: {:?})",
            model,
            max_tokens
        );

        let request = CodestralRequest {
            model,
            prompt,
            suffix: if suffix.is_empty() {
                None
            } else {
                Some(suffix)
            },
            max_tokens: max_tokens.or(Some(350)),
            temperature: Some(0.2),
            top_p: Some(1.0),
            stream: Some(false),
            stop: None,
            random_seed: None,
            min_tokens: None,
        };

        let request_body = serde_json::to_string(&request)?;

        log::debug!("Codestral: Sending FIM request");

        let http_request = http_client::Request::builder()
            .method(http_client::Method::POST)
            .uri(format!("{}/v1/fim/completions", api_url))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", api_key))
            .body(http_client::AsyncBody::from(request_body))?;

        let mut response = http_client.send(http_request).await?;
        let status = response.status();

        log::debug!("Codestral: Response status: {}", status);

        if !status.is_success() {
            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;
            return Err(anyhow::anyhow!(
                "Codestral API error: {} - {}",
                status,
                body
            ));
        }

        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;

        let codestral_response: CodestralResponse = serde_json::from_str(&body)?;

        let elapsed = start_time.elapsed();

        if let Some(choice) = codestral_response.choices.first() {
            let completion = &choice.message.content;

            log::debug!(
                "Codestral: Completion received ({} tokens, {:.2}s)",
                codestral_response.usage.completion_tokens,
                elapsed.as_secs_f64()
            );

            // Return just the completion text for insertion at cursor
            Ok(completion.clone())
        } else {
            log::error!("Codestral: No completion returned in response");
            Err(anyhow::anyhow!("No completion returned from Codestral"))
        }
    }
}

impl EditPredictionDelegate for CodestralEditPredictionDelegate {
    fn name() -> &'static str {
        "codestral"
    }

    fn display_name() -> &'static str {
        "Codestral"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::AiMistral)
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        codestral_api_key(cx).is_some()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: language::Anchor,
        debounce_duration: Duration,
        _trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        log::debug!(
            "Codestral: Refresh called (debounce_duration: {:?})",
            debounce_duration
        );

        let Some(api_key) = codestral_api_key(cx) else {
            log::warn!("Codestral: No API key configured, skipping refresh");
            return;
        };

        let snapshot = buffer.read(cx).snapshot();

        // Check if current completion is still valid
        if let Some(current_completion) = self.current_completion.as_ref()
            && current_completion.interpolate(&snapshot).is_some()
        {
            return;
        }

        let http_client = self.http_client.clone();

        // Get settings
        let settings = all_language_settings(None, cx);
        let model = settings
            .edit_predictions
            .codestral
            .model
            .clone()
            .unwrap_or_else(|| "codestral-latest".to_string());
        let max_tokens = settings.edit_predictions.codestral.max_tokens;
        let api_url = codestral_api_url(cx).to_string();
        // zed-jp-enhance (`docs/jp-enhance/DESIGN.md` ADR-F10): an API key alone, even one only in
        // the environment, is not consent to send code off this machine.
        if !edit_prediction::endpoint_is_local_or_acknowledged(
            edit_prediction::CODESTRAL_CONSENT_ID,
            &api_url,
            cx,
        ) {
            log::debug!(
                "Codestral: not requesting a completion from {api_url}: the external AI warning \
                 for Codestral edit predictions has not been accepted"
            );
            return;
        }

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            if !debounce_duration.is_zero() {
                log::debug!("Codestral: Debouncing for {:?}", debounce_duration);
                cx.background_executor().timer(debounce_duration).await;
            }

            let cursor_offset = cursor_position.to_offset(&snapshot);

            const MAX_EDITABLE_TOKENS: usize = 350;
            const MAX_CONTEXT_TOKENS: usize = 150;

            let (excerpt_point_range, excerpt_offset_range, cursor_offset_in_excerpt) =
                cursor_excerpt::compute_cursor_excerpt(&snapshot, cursor_offset);
            let syntax_ranges = cursor_excerpt::compute_syntax_ranges(
                &snapshot,
                cursor_offset,
                &excerpt_offset_range,
            );
            let excerpt_text: String = snapshot.text_for_range(excerpt_point_range).collect();
            let (_, context_range) = zeta_prompt::compute_editable_and_context_ranges(
                &excerpt_text,
                cursor_offset_in_excerpt,
                &syntax_ranges,
                MAX_EDITABLE_TOKENS,
                MAX_CONTEXT_TOKENS,
            );
            let context_text = &excerpt_text[context_range.clone()];
            let cursor_within_excerpt = cursor_offset_in_excerpt
                .saturating_sub(context_range.start)
                .min(context_text.len());
            let prompt = context_text[..cursor_within_excerpt].to_string();
            let suffix = context_text[cursor_within_excerpt..].to_string();

            let completion_text = match Self::fetch_completion(
                http_client,
                &api_key,
                prompt,
                suffix,
                model,
                max_tokens,
                api_url,
            )
            .await
            {
                Ok(completion) => completion,
                Err(e) => {
                    log::error!("Codestral: Failed to fetch completion: {}", e);
                    this.update(cx, |this, cx| {
                        this.pending_request = None;
                        cx.notify();
                    })?;
                    return Err(e);
                }
            };

            if completion_text.trim().is_empty() {
                log::debug!("Codestral: Completion was empty after trimming; ignoring");
                this.update(cx, |this, cx| {
                    this.pending_request = None;
                    cx.notify();
                })?;
                return Ok(());
            }

            let edits: Arc<[(Range<Anchor>, Arc<str>)]> =
                vec![(cursor_position..cursor_position, completion_text.into())].into();
            let edit_preview = buffer
                .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                .await;

            this.update(cx, |this, cx| {
                this.current_completion = Some(CurrentCompletion {
                    snapshot,
                    edits,
                    edit_preview,
                });
                this.pending_request = None;
                cx.notify();
            })?;

            Ok(())
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        log::debug!("Codestral: Completion accepted");
        self.pending_request = None;
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        log::debug!("Codestral: Completion discarded");
        self.pending_request = None;
        self.current_completion = None;
    }

    /// Returns the completion suggestion, adjusted or invalidated based on user edits
    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let current_completion = self.current_completion.as_ref()?;
        let buffer = buffer.read(cx);
        let edits = current_completion.interpolate(&buffer.snapshot())?;
        if edits.is_empty() {
            return None;
        }
        Some(EditPrediction::Local {
            id: None,
            edits,
            cursor_position: None,
            edit_preview: Some(current_completion.edit_preview.clone()),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodestralRequest {
    pub model: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub random_seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CodestralResponse {
    pub id: String,
    pub object: String,
    pub model: String,
    pub usage: Usage,
    pub created: u64,
    pub choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub content: String,
    pub role: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AsyncApp, TestAppContext, UpdateGlobal as _};
    use http_client::FakeHttpClient;
    use settings::SettingsStore;
    use std::{future::Future, pin::Pin, sync::Mutex};

    struct FakeCodestralCredentials;

    impl credentials_provider::CredentialsProvider for FakeCodestralCredentials {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async { Ok(Some(("Bearer".to_string(), b"test-key".to_vec()))) })
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

    fn set_codestral_settings(api_url: Option<&str>, acknowledged: &[&str], cx: &mut App) {
        let api_url = api_url.map(str::to_string);
        let acknowledged = acknowledged
            .iter()
            .map(|key| key.to_string())
            .collect::<Vec<_>>();
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                let edit_predictions = settings
                    .project
                    .all_languages
                    .edit_predictions
                    .get_or_insert_default();
                edit_predictions.provider = Some(settings::EditPredictionProvider::Codestral);
                edit_predictions.codestral.get_or_insert_default().api_url = api_url;
                settings.acknowledged_external_ai_providers = Some(acknowledged);
            });
        });
    }

    async fn requested_hosts_after_refresh(
        api_url: Option<&str>,
        acknowledged: &[&str],
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(zed_credentials_provider::ZedCredentialsProvider(Arc::new(
                FakeCodestralCredentials,
            )));
            set_codestral_settings(api_url, acknowledged, cx);
        });
        cx.update(load_codestral_api_key)
            .await
            .expect("the fake credentials provider has a key");

        let requested_hosts = Arc::new(Mutex::new(Vec::new()));
        let http_client = FakeHttpClient::create({
            let requested_hosts = requested_hosts.clone();
            move |request| {
                requested_hosts
                    .lock()
                    .expect("request log")
                    .push(request.uri().host().unwrap_or_default().to_string());
                async move {
                    Ok(http_client::Response::builder()
                        .status(404)
                        .body("Not Found".into())?)
                }
            }
        });
        let buffer = cx.new(|cx| Buffer::local("fn main() {\n    \n}\n", cx));
        let cursor = buffer.read_with(cx, |buffer, _| buffer.anchor_before(16));
        let delegate = cx.new(|_| CodestralEditPredictionDelegate::new(http_client));
        delegate.update(cx, |delegate, cx| {
            delegate.refresh(
                buffer,
                cursor,
                Duration::ZERO,
                EditPredictionRequestTrigger::Explicit,
                cx,
            )
        });
        cx.run_until_parked();
        requested_hosts.lock().expect("request log").clone()
    }

    /// An API key, which the environment alone can supply, is not consent to send code to
    /// Codestral (zed-jp-enhance ADR-F10).
    #[gpui::test]
    async fn test_codestral_waits_for_consent(cx: &mut TestAppContext) {
        assert_eq!(
            requested_hosts_after_refresh(None, &[], cx).await,
            Vec::<String>::new()
        );
        assert_eq!(
            requested_hosts_after_refresh(
                Some("https://codestral.corp.example"),
                &["codestral", "mistral", "edit_prediction:ollama"],
                cx
            )
            .await,
            Vec::<String>::new(),
            "only the edit prediction consent for Codestral opens it"
        );
        assert_eq!(
            requested_hosts_after_refresh(None, &["edit_prediction:codestral"], cx).await,
            vec!["codestral.mistral.ai".to_string()]
        );
        assert_eq!(
            requested_hosts_after_refresh(Some("http://127.0.0.1:8080"), &[], cx).await,
            vec!["127.0.0.1".to_string()],
            "a server on this machine needs no consent"
        );
    }
}
