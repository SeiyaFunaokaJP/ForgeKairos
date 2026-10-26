//! The built-in browser's tools (ADR-F89). The same tools are offered over MCP by `ide_server`;
//! their names, descriptions, schemas and argument parsing all come from `browser_control`, so
//! the two cannot drift apart. They are only offered while the user has turned
//! `browser.ai_control` on (see [`browser_tools_enabled`]).

use std::sync::{Arc, LazyLock};

use agent_client_protocol::schema::v2 as acp;
use browser_control::{
    BrowserCommand, BrowserControl, ElementTarget, NavigateTarget, ScrollDirection, ToolDefinition,
};
use futures::FutureExt as _;
use gpui::{App, Entity, Image, ImageFormat, SharedString, Task};
use language_model::{
    LanguageModelImage, LanguageModelImageExt as _, LanguageModelToolResultContent,
};
use project::Project;
use schemars::Schema;
use serde::{Deserialize, Serialize};
use util::markdown::{MarkdownCodeBlock, MarkdownInlineCode};

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput, ToolPermissionContext};

const CANCELLED_MESSAGE: &str = "The browser tool was cancelled by the user";

/// Longest text typed or evaluated that a tool call's title shows.
const MAX_TITLE_TEXT_CHARS: usize = 200;

static DEFINITIONS: LazyLock<Vec<ToolDefinition>> =
    LazyLock::new(browser_control::tool_definitions);

fn definition(name: &str) -> Option<&'static ToolDefinition> {
    DEFINITIONS
        .iter()
        .find(|definition| definition.name == name)
}

/// Whether the browser tools are offered to the model: the built-in browser registered itself and
/// the user turned AI control of it on.
pub fn browser_tools_enabled(cx: &App) -> bool {
    enabled_browser_control(cx).is_some()
}

pub fn is_browser_tool(name: &str) -> bool {
    browser_control::TOOL_NAMES.contains(&name)
}

fn enabled_browser_control(cx: &App) -> Option<Arc<dyn BrowserControl>> {
    browser_control::browser_control(cx).filter(|control| control.is_enabled(cx))
}

fn tool_description(name: &str) -> SharedString {
    definition(name)
        .map(|definition| SharedString::new_static(definition.description))
        .unwrap_or_default()
}

fn tool_input_schema(name: &str) -> Schema {
    definition(name)
        .and_then(|definition| Schema::try_from(definition.input_schema.clone()).ok())
        .unwrap_or_else(|| {
            log::error!("browser tool {name} has no input schema in browser_control");
            schemars::json_schema!({ "type": "object", "properties": {} })
        })
}

/// What a browser tool returns: text, and for `browser_screenshot` the image as well.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BrowserToolOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<LanguageModelImage>,
}

impl BrowserToolOutput {
    fn failure(message: impl Into<String>) -> Self {
        Self {
            text: message.into(),
            image: None,
        }
    }

    fn into_llm_output(self) -> Vec<LanguageModelToolResultContent> {
        let mut output = Vec::new();
        if !self.text.is_empty() || self.image.is_none() {
            output.push(LanguageModelToolResultContent::Text(self.text.into()));
        }
        if let Some(image) = self.image {
            output.push(LanguageModelToolResultContent::Image(image));
        }
        output
    }
}

impl From<BrowserToolOutput> for LanguageModelToolResultContent {
    fn from(output: BrowserToolOutput) -> Self {
        LanguageModelToolResultContent::Text(output.text.into())
    }
}

macro_rules! browser_tools {
    ($($tool:ident = $name:literal, $kind:ident;)*) => {
        $(
            #[doc = concat!("The `", $name, "` browser tool, as `browser_control` defines it.")]
            pub struct $tool {
                project: Entity<Project>,
            }

            impl $tool {
                pub fn new(project: Entity<Project>) -> Self {
                    Self { project }
                }
            }

            impl AgentTool for $tool {
                type Input = serde_json::Value;
                type Output = BrowserToolOutput;

                const NAME: &'static str = $name;

                fn description() -> SharedString {
                    tool_description(Self::NAME)
                }

                fn input_schema() -> Schema {
                    tool_input_schema(Self::NAME)
                }

                fn kind() -> acp::ToolKind {
                    acp::ToolKind::$kind
                }

                fn allow_in_restricted_mode() -> bool {
                    false
                }

                fn initial_title(
                    &self,
                    input: Result<Self::Input, serde_json::Value>,
                    _cx: &mut App,
                ) -> SharedString {
                    let input = input.unwrap_or_else(|input| input);
                    browser_tool_title(Self::NAME, input).into()
                }

                fn llm_output(output: Self::Output) -> Vec<LanguageModelToolResultContent> {
                    output.into_llm_output()
                }

                fn run(
                    self: Arc<Self>,
                    input: ToolInput<Self::Input>,
                    event_stream: ToolCallEventStream,
                    cx: &mut App,
                ) -> Task<Result<Self::Output, Self::Output>> {
                    run_browser_tool(Self::NAME, self.project.clone(), input, event_stream, cx)
                }

                fn replay(
                    &self,
                    _input: Self::Input,
                    output: Self::Output,
                    event_stream: ToolCallEventStream,
                    _cx: &mut App,
                ) -> anyhow::Result<()> {
                    show_output(&output, &event_stream);
                    Ok(())
                }
            }
        )*

        /// The agent's browser tools, in the order of `browser_control::TOOL_NAMES`.
        pub const BROWSER_TOOL_NAMES: &[&str] = &[$($name,)*];

        pub(crate) fn add_browser_tools(thread: &mut Thread) {
            let project = thread.project().clone();
            $(thread.add_tool($tool::new(project.clone()));)*
        }

        #[cfg(test)]
        fn erased_browser_tools(project: Entity<Project>) -> Vec<Arc<dyn crate::AnyAgentTool>> {
            vec![$(AgentTool::erase($tool::new(project.clone())),)*]
        }
    };
}

browser_tools! {
    BrowserTabsTool = "browser_tabs", Read;
    BrowserOpenTabTool = "browser_open_tab", Fetch;
    BrowserCloseTabTool = "browser_close_tab", Other;
    BrowserSelectTabTool = "browser_select_tab", Other;
    BrowserNavigateTool = "browser_navigate", Fetch;
    BrowserPageTextTool = "browser_page_text", Read;
    BrowserSnapshotTool = "browser_snapshot", Read;
    BrowserFindTool = "browser_find", Search;
    BrowserClickTool = "browser_click", Other;
    BrowserHoverTool = "browser_hover", Other;
    BrowserTypeTool = "browser_type", Other;
    BrowserPressKeyTool = "browser_press_key", Other;
    BrowserScrollTool = "browser_scroll", Other;
    BrowserSetValueTool = "browser_set_value", Other;
    BrowserScreenshotTool = "browser_screenshot", Read;
    BrowserConsoleTool = "browser_console", Read;
    BrowserNetworkTool = "browser_network", Read;
    // Not `Execute`: that kind is kept for the tools that write into worktrees, and a page's
    // script cannot.
    BrowserEvalTool = "browser_eval", Other;
    BrowserWaitForTool = "browser_wait_for", Read;
}

/// The browser tools that act on the browser (open, navigate, click, type, run scripts, ...)
/// rather than only read it. They go through the user's tool permissions like `fetch` does, so
/// each has its own entry in the tool permission settings.
pub const BROWSER_ACTING_TOOL_NAMES: &[&str] = &[
    "browser_open_tab",
    "browser_close_tab",
    "browser_select_tab",
    "browser_navigate",
    "browser_click",
    "browser_hover",
    "browser_type",
    "browser_press_key",
    "browser_scroll",
    "browser_set_value",
    "browser_eval",
];

fn run_browser_tool(
    name: &'static str,
    project: Entity<Project>,
    input: ToolInput<serde_json::Value>,
    event_stream: ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<BrowserToolOutput, BrowserToolOutput>> {
    cx.spawn(async move |cx| {
        let input = input.recv().await.map_err(|error| {
            BrowserToolOutput::failure(format!("Failed to receive tool input: {error}"))
        })?;
        // The setting may have been turned off since the tools were handed to the model.
        let control = cx
            .update(|cx| enabled_browser_control(cx))
            .ok_or_else(|| BrowserToolOutput::failure(browser_control::AI_CONTROL_OFF_MESSAGE))?;
        let command = browser_control::parse_command(name, input)
            .map_err(|error| BrowserToolOutput::failure(format!("{error:#}")))?;

        if let Some(permission_input) = permission_input(&command) {
            let authorize = cx.update(|cx| {
                event_stream.authorize(
                    command_title(&command),
                    ToolPermissionContext::new(name, vec![permission_input]),
                    cx,
                )
            });
            futures::select! {
                result = authorize.fuse() => {
                    result.map_err(|error| BrowserToolOutput::failure(error.to_string()))?
                }
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(BrowserToolOutput::failure(CANCELLED_MESSAGE));
                }
            };
            // The user may have turned AI control off while the permission card was open.
            if !cx.update(|cx| control.is_enabled(cx)) {
                return Err(BrowserToolOutput::failure(
                    browser_control::AI_CONTROL_OFF_MESSAGE,
                ));
            }
        }

        let run = cx.update(|cx| control.run(&project, command, cx));
        let output = futures::select! {
            result = run.fuse() => {
                result.map_err(|error| BrowserToolOutput::failure(format!("{error:#}")))?
            }
            _ = event_stream.cancelled_by_user().fuse() => {
                return Err(BrowserToolOutput::failure(CANCELLED_MESSAGE));
            }
        };
        let image = match output.image_png {
            Some(png) => {
                let image = Arc::new(Image::from_bytes(ImageFormat::Png, png));
                let image = cx
                    .update(|cx| LanguageModelImage::from_image(image, cx))
                    .await
                    .ok_or_else(|| {
                        BrowserToolOutput::failure(
                            "The screenshot could not be turned into an image for the model",
                        )
                    })?;
                Some(image)
            }
            None => None,
        };
        let output = BrowserToolOutput {
            text: output.text,
            image,
        };
        show_output(&output, &event_stream);
        Ok(output)
    })
}

/// The text the user's tool-permission rules match for a command that acts on the browser, or
/// `None` for one that only reads it, which runs without asking.
fn permission_input(command: &BrowserCommand) -> Option<String> {
    match command {
        BrowserCommand::ListTabs
        | BrowserCommand::PageText { .. }
        | BrowserCommand::Snapshot { .. }
        | BrowserCommand::Find { .. }
        | BrowserCommand::Screenshot { .. }
        | BrowserCommand::Console { .. }
        | BrowserCommand::Network { .. }
        | BrowserCommand::WaitFor { .. } => None,
        BrowserCommand::OpenTab { url } => {
            Some(url.clone().unwrap_or_else(|| "about:blank".to_string()))
        }
        BrowserCommand::CloseTab { tab } | BrowserCommand::SelectTab { tab } => {
            Some(tab.to_string())
        }
        BrowserCommand::Navigate { target, .. } => Some(match target {
            NavigateTarget::Url(url) => url.clone(),
            NavigateTarget::Back => "back".to_string(),
            NavigateTarget::Forward => "forward".to_string(),
            NavigateTarget::Reload => "reload".to_string(),
        }),
        BrowserCommand::Click { target, .. } | BrowserCommand::Hover { target, .. } => {
            Some(match target {
                ElementTarget::Ref(element) => element.clone(),
                ElementTarget::Point { x, y } => format!("{x},{y}"),
            })
        }
        BrowserCommand::Type { text, .. } => Some(text.clone()),
        BrowserCommand::PressKey { keys, .. } => Some(keys.clone()),
        BrowserCommand::Scroll { direction, .. } => Some(
            match direction {
                ScrollDirection::Up => "up",
                ScrollDirection::Down => "down",
                ScrollDirection::Left => "left",
                ScrollDirection::Right => "right",
            }
            .to_string(),
        ),
        BrowserCommand::SetValue { value, .. } => Some(match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        }),
        BrowserCommand::Eval { expression, .. } => Some(expression.clone()),
    }
}

fn browser_tool_title(name: &str, input: serde_json::Value) -> String {
    match browser_control::parse_command(name, input) {
        Ok(command) => command_title(&command),
        Err(_) => generic_title(name).to_string(),
    }
}

fn shortened(text: &str) -> String {
    let mut characters = text.chars();
    let mut shortened = characters
        .by_ref()
        .take(MAX_TITLE_TEXT_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        shortened.push('…');
    }
    shortened
}

fn target_label(target: &ElementTarget) -> String {
    match target {
        ElementTarget::Ref(element) => MarkdownInlineCode(element).to_string(),
        ElementTarget::Point { x, y } => format!("({x}, {y})"),
    }
}

fn command_title(command: &BrowserCommand) -> String {
    match command {
        BrowserCommand::ListTabs => i18n::t!("List the browser's AI tabs").to_string(),
        BrowserCommand::OpenTab { url: Some(url) } => {
            i18n::t!("Open {} in a new AI tab", MarkdownInlineCode(url))
        }
        BrowserCommand::OpenTab { url: None } => i18n::t!("Open a new AI tab").to_string(),
        BrowserCommand::CloseTab { tab } => i18n::t!("Close AI tab {}", tab),
        BrowserCommand::SelectTab { tab } => i18n::t!("Switch to AI tab {}", tab),
        BrowserCommand::Navigate { target, .. } => match target {
            NavigateTarget::Url(url) => i18n::t!("Navigate to {}", MarkdownInlineCode(url)),
            NavigateTarget::Back => i18n::t!("Go back in the browser").to_string(),
            NavigateTarget::Forward => i18n::t!("Go forward in the browser").to_string(),
            NavigateTarget::Reload => i18n::t!("Reload the page").to_string(),
        },
        BrowserCommand::PageText { .. } => i18n::t!("Read the page's text").to_string(),
        BrowserCommand::Snapshot { .. } => i18n::t!("Take a snapshot of the page").to_string(),
        BrowserCommand::Find { query, .. } => {
            i18n::t!("Find {} on the page", MarkdownInlineCode(&shortened(query)))
        }
        BrowserCommand::Click { target, .. } => i18n::t!("Click {}", target_label(target)),
        BrowserCommand::Hover { target, .. } => i18n::t!("Hover over {}", target_label(target)),
        BrowserCommand::Type { text, .. } => {
            i18n::t!("Type {}", MarkdownInlineCode(&shortened(text)))
        }
        BrowserCommand::PressKey { keys, .. } => {
            i18n::t!("Press {}", MarkdownInlineCode(&shortened(keys)))
        }
        BrowserCommand::Scroll { .. } => i18n::t!("Scroll the page").to_string(),
        BrowserCommand::SetValue { element, .. } => {
            i18n::t!("Set the value of {}", MarkdownInlineCode(element))
        }
        BrowserCommand::Screenshot { .. } => i18n::t!("Take a screenshot of the page").to_string(),
        BrowserCommand::Console { .. } => i18n::t!("Read the page's console").to_string(),
        BrowserCommand::Network { .. } => i18n::t!("List the page's network requests").to_string(),
        BrowserCommand::Eval { expression, .. } => {
            i18n::t!(
                "Run {} in the page",
                MarkdownInlineCode(&shortened(expression))
            )
        }
        BrowserCommand::WaitFor { .. } => i18n::t!("Wait for the page").to_string(),
    }
}

/// The title of a call whose arguments are not (yet) valid.
fn generic_title(name: &str) -> &'static str {
    match name {
        "browser_tabs" => i18n::t!("List the browser's AI tabs"),
        "browser_open_tab" => i18n::t!("Open a new AI tab"),
        "browser_close_tab" => i18n::t!("Close an AI tab"),
        "browser_select_tab" => i18n::t!("Switch AI tabs"),
        "browser_navigate" => i18n::t!("Navigate the browser"),
        "browser_page_text" => i18n::t!("Read the page's text"),
        "browser_snapshot" => i18n::t!("Take a snapshot of the page"),
        "browser_find" => i18n::t!("Find on the page"),
        "browser_click" => i18n::t!("Click on the page"),
        "browser_hover" => i18n::t!("Hover on the page"),
        "browser_type" => i18n::t!("Type into the page"),
        "browser_press_key" => i18n::t!("Press keys in the page"),
        "browser_scroll" => i18n::t!("Scroll the page"),
        "browser_set_value" => i18n::t!("Set a form value"),
        "browser_screenshot" => i18n::t!("Take a screenshot of the page"),
        "browser_console" => i18n::t!("Read the page's console"),
        "browser_network" => i18n::t!("List the page's network requests"),
        "browser_eval" => i18n::t!("Run JavaScript in the page"),
        "browser_wait_for" => i18n::t!("Wait for the page"),
        _ => i18n::t!("Use the browser"),
    }
}

fn show_output(output: &BrowserToolOutput, event_stream: &ToolCallEventStream) {
    let mut content = Vec::new();
    if !output.text.is_empty() {
        let text = MarkdownCodeBlock {
            tag: "",
            text: &output.text,
        }
        .to_string();
        content.push(acp::ToolCallContent::Content(Box::new(acp::Content::new(
            text,
        ))));
    }
    if let Some(image) = &output.image {
        content.push(acp::ToolCallContent::Content(Box::new(acp::Content::new(
            acp::ContentBlock::Image(acp::ImageContent::new(image.source.clone(), "image/png")),
        ))));
    }
    if !content.is_empty() {
        event_stream.update_fields(|update| update.content(content));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCallEventStream;
    use gpui::{EntityId, TestAppContext, UpdateGlobal as _};
    use language_model::LanguageModelToolResultContent;
    use parking_lot::Mutex;
    use project::FakeFs;
    use serde_json::json;
    use settings::{Settings as _, SettingsStore};
    use std::sync::atomic::{AtomicBool, Ordering};
    use util::path;

    #[derive(Default)]
    struct FakeBrowser {
        enabled: AtomicBool,
        calls: Mutex<Vec<(EntityId, BrowserCommand)>>,
        reply: Mutex<Option<Result<browser_control::BrowserOutput, String>>>,
    }

    impl BrowserControl for FakeBrowser {
        fn is_enabled(&self, _cx: &App) -> bool {
            self.enabled.load(Ordering::SeqCst)
        }

        fn run(
            &self,
            project: &Entity<Project>,
            command: BrowserCommand,
            _cx: &mut App,
        ) -> Task<anyhow::Result<browser_control::BrowserOutput>> {
            self.calls.lock().push((project.entity_id(), command));
            let reply = self.reply.lock().clone().unwrap_or_else(|| {
                Ok(browser_control::BrowserOutput {
                    text: "done".to_string(),
                    image_png: None,
                })
            });
            Task::ready(reply.map_err(|message| anyhow::anyhow!(message)))
        }
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeBrowser> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            project::DisableAiSettings::register(cx);
            agent_settings::AgentSettings::register(cx);
        });
        install_browser(true, cx)
    }

    fn install_browser(enabled: bool, cx: &mut TestAppContext) -> Arc<FakeBrowser> {
        let browser = Arc::new(FakeBrowser::default());
        browser.enabled.store(enabled, Ordering::SeqCst);
        cx.update(|cx| cx.set_global(browser_control::GlobalBrowserControl(browser.clone())));
        browser
    }

    fn set_permission_default(mode: settings::ToolPermissionMode, cx: &mut TestAppContext) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .tool_permissions
                        .get_or_insert_default()
                        .default = Some(mode);
                });
            });
        });
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "index.html": "<p>hi</p>" }))
            .await;
        Project::test(fs, [path!("/root").as_ref()], cx).await
    }

    fn run<T: AgentTool<Input = serde_json::Value, Output = BrowserToolOutput>>(
        tool: T,
        input: serde_json::Value,
        event_stream: ToolCallEventStream,
        cx: &mut TestAppContext,
    ) -> Task<Result<BrowserToolOutput, BrowserToolOutput>> {
        let tool = Arc::new(tool);
        cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
    }

    fn property_names(schema: &serde_json::Value) -> Vec<String> {
        let mut names = schema["properties"]
            .as_object()
            .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn required_names(schema: &serde_json::Value) -> Vec<String> {
        let mut names = schema["required"]
            .as_array()
            .map(|required| {
                required
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn minimal_input(name: &str) -> serde_json::Value {
        match name {
            "browser_close_tab" | "browser_select_tab" => json!({ "tab": "1" }),
            "browser_navigate" => json!({ "url": "http://localhost:3000/" }),
            "browser_find" => json!({ "query": "Sign in" }),
            "browser_click" | "browser_hover" => json!({ "ref": "e1" }),
            "browser_type" => json!({ "text": "hello" }),
            "browser_press_key" => json!({ "keys": "Enter" }),
            "browser_scroll" => json!({ "direction": "down" }),
            "browser_set_value" => json!({ "ref": "e1", "value": "x" }),
            "browser_eval" => json!({ "expression": "document.title" }),
            "browser_wait_for" => json!({ "text": "Done" }),
            _ => json!({}),
        }
    }

    #[test]
    fn every_browser_tool_of_the_contract_is_an_agent_tool() {
        assert_eq!(
            BROWSER_TOOL_NAMES,
            browser_control::tool_names().collect::<Vec<_>>().as_slice()
        );
        for name in BROWSER_TOOL_NAMES {
            assert!(
                crate::ALL_TOOL_NAMES.contains(name),
                "{name} is missing from the tools! list"
            );
            assert!(is_browser_tool(name));
            assert!(
                !crate::tool_allowed_in_restricted_mode(name),
                "{name} must not reach a restricted workspace"
            );
        }
        let browser_like = crate::ALL_TOOL_NAMES
            .iter()
            .filter(|name| name.starts_with("browser_"))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(browser_like, BROWSER_TOOL_NAMES);
    }

    #[test]
    fn the_model_gets_the_contract_schemas_and_descriptions() {
        let built_in = crate::built_in_tools().collect::<Vec<_>>();
        for definition in browser_control::tool_definitions() {
            let tool = built_in
                .iter()
                .find(|tool| tool.name == definition.name)
                .unwrap_or_else(|| panic!("{} is a built-in tool", definition.name));
            let language_model::LanguageModelRequestToolInput::Function { input_schema, .. } =
                &tool.input
            else {
                panic!("{} should use a JSON schema", definition.name);
            };
            assert_eq!(
                property_names(input_schema),
                property_names(&definition.input_schema),
                "{}",
                definition.name
            );
            assert_eq!(
                required_names(input_schema),
                required_names(&definition.input_schema),
                "{}",
                definition.name
            );
            assert_eq!(
                input_schema, &definition.input_schema,
                "{}: the schema is passed on unchanged",
                definition.name
            );
            assert_eq!(tool.description, definition.description);
        }
    }

    /// `write` has every browser tool, `ask` only those that read the page, and the profiles that
    /// never reach the network have none.
    #[gpui::test]
    fn test_browser_tools_in_the_built_in_profiles(cx: &mut App) {
        use agent_settings::{AgentProfileId, AgentSettings, builtin_profiles};

        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let profiles = &AgentSettings::get_global(cx).profiles;
        for profile_id in [
            builtin_profiles::WRITE,
            builtin_profiles::ASK,
            builtin_profiles::EDIT,
            builtin_profiles::MINIMAL,
        ] {
            let profile = profiles
                .get(&AgentProfileId(profile_id.into()))
                .expect("the profile ships in default.json");
            for tool in BROWSER_TOOL_NAMES {
                let expected = if profile_id == builtin_profiles::WRITE {
                    true
                } else if profile_id == builtin_profiles::ASK {
                    !BROWSER_ACTING_TOOL_NAMES.contains(tool)
                } else {
                    false
                };
                assert_eq!(
                    profile.is_tool_enabled(tool),
                    expected,
                    "{tool} in the {profile_id} profile"
                );
            }
        }
    }

    #[test]
    fn only_the_acting_tools_ask_for_permission() {
        for name in BROWSER_TOOL_NAMES {
            let command = browser_control::parse_command(name, minimal_input(name))
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert_eq!(
                permission_input(&command).is_some(),
                BROWSER_ACTING_TOOL_NAMES.contains(name),
                "{name}"
            );
        }
    }

    #[gpui::test]
    async fn the_tools_are_offered_only_while_ai_control_is_on(cx: &mut TestAppContext) {
        assert!(
            !cx.update(|cx| crate::tool_feature_flag_enabled("browser_tabs", cx)),
            "no browser has registered itself"
        );
        let browser = install_browser(false, cx);
        for name in BROWSER_TOOL_NAMES {
            assert!(!cx.update(|cx| crate::tool_feature_flag_enabled(name, cx)));
        }
        browser.enabled.store(true, Ordering::SeqCst);
        for name in BROWSER_TOOL_NAMES {
            assert!(cx.update(|cx| crate::tool_feature_flag_enabled(name, cx)));
        }
        assert!(cx.update(|cx| crate::tool_feature_flag_enabled("read_file", cx)));
    }

    #[gpui::test]
    async fn a_read_only_tool_runs_without_asking(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        let (event_stream, mut receiver) = ToolCallEventStream::test();

        let output = run(
            BrowserFindTool::new(project.clone()),
            json!({ "query": "Save", "limit": 500 }),
            event_stream,
            cx,
        )
        .await
        .expect("the tool succeeds");
        assert_eq!(output.text, "done");
        receiver.expect_no_authorization();
        assert_eq!(
            *browser.calls.lock(),
            vec![(
                project.entity_id(),
                BrowserCommand::Find {
                    tab: None,
                    query: "Save".to_string(),
                    limit: browser_control::MAX_FIND_LIMIT,
                }
            )]
        );
    }

    #[gpui::test]
    async fn an_acting_tool_asks_with_its_input_and_a_denial_stops_it(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        let (event_stream, mut receiver) = ToolCallEventStream::test();

        let task = run(
            BrowserNavigateTool::new(project.clone()),
            json!({ "url": "https://example.com/app" }),
            event_stream,
            cx,
        );
        let authorization = receiver.expect_authorization().await;
        assert_eq!(
            authorization.tool_call.title.value().map(String::as_str),
            Some("Navigate to `https://example.com/app`")
        );
        let context = authorization
            .context
            .as_ref()
            .expect("a permission context");
        assert_eq!(context.tool_name, "browser_navigate");
        assert_eq!(
            context.input_values,
            vec!["https://example.com/app".to_string()]
        );
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("deny"),
                acp::PermissionOptionKind::RejectOnce,
            ))
            .expect("the tool waits for the answer");
        assert!(task.await.is_err(), "a denial fails the tool");
        assert_eq!(
            *browser.calls.lock(),
            Vec::new(),
            "nothing reached the browser"
        );

        // Allowed by the tool permissions, the same call runs without asking.
        set_permission_default(settings::ToolPermissionMode::Allow, cx);
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let output = run(
            BrowserNavigateTool::new(project.clone()),
            json!({ "url": "https://example.com/app" }),
            event_stream,
            cx,
        )
        .await
        .expect("the tool succeeds");
        assert_eq!(output.text, "done");
        receiver.expect_no_authorization();
        assert_eq!(browser.calls.lock().len(), 1);
    }

    #[gpui::test]
    async fn a_screenshot_reaches_the_model_as_text_and_image(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        // A 1x1 PNG.
        let png = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0x64, 0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47,
            0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        *browser.reply.lock() = Some(Ok(browser_control::BrowserOutput {
            text: "Viewport: 1280x720 CSS px".to_string(),
            image_png: Some(png),
        }));
        let (event_stream, _receiver) = ToolCallEventStream::test();

        let output = run(
            BrowserScreenshotTool::new(project),
            json!({}),
            event_stream,
            cx,
        )
        .await
        .expect("the tool succeeds");
        let image = output.image.clone().expect("the screenshot has an image");
        assert!(!image.is_empty());
        let parts = BrowserScreenshotTool::llm_output(output.clone());
        assert_eq!(
            parts,
            vec![
                LanguageModelToolResultContent::Text("Viewport: 1280x720 CSS px".into()),
                LanguageModelToolResultContent::Image(image),
            ]
        );

        // The output survives being stored with the thread.
        let stored = serde_json::to_value(&output).expect("the output serializes");
        let restored: BrowserToolOutput =
            serde_json::from_value(stored).expect("the output deserializes");
        assert_eq!(restored, output);

        // The thread runs tools through their erased form, which is what has to call
        // `llm_output` rather than the text-only `Into`.
        let (event_stream, _receiver) = ToolCallEventStream::test();
        let project = test_project(cx).await;
        let erased = crate::AgentTool::erase(BrowserScreenshotTool::new(project));
        let erased_output = cx
            .update(|cx| erased.run(ToolInput::resolved(json!({})), event_stream, cx))
            .await
            .unwrap_or_else(|_| panic!("the erased tool succeeds"));
        assert_eq!(erased_output.llm_output, parts);
        let stored: BrowserToolOutput =
            serde_json::from_value(erased_output.raw_output).expect("the raw output is stored");
        assert_eq!(stored, output);
    }

    /// Run as the thread runs them, every tool that acts on the browser asks first, with the
    /// input its permission rules match, and nothing reaches the browser when the user says no;
    /// the ones that only read run without asking.
    #[gpui::test]
    async fn every_acting_tool_asks_before_it_reaches_the_browser(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        let tools = erased_browser_tools(project);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>(),
            BROWSER_TOOL_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        );

        for tool in tools {
            let name = tool.name().to_string();
            let input = minimal_input(&name);
            let command = browser_control::parse_command(&name, input.clone())
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            let (event_stream, mut receiver) = ToolCallEventStream::test();
            let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

            if BROWSER_ACTING_TOOL_NAMES.contains(&name.as_str()) {
                let authorization = receiver.expect_authorization().await;
                let context = authorization
                    .context
                    .as_ref()
                    .unwrap_or_else(|| panic!("{name}: a permission context"));
                assert_eq!(context.tool_name, name);
                assert_eq!(
                    Some(context.input_values.clone()),
                    permission_input(&command).map(|input| vec![input]),
                    "{name}"
                );
                authorization
                    .response
                    .send(acp_thread::SelectedPermissionOutcome::new(
                        acp::PermissionOptionId::new("deny"),
                        acp::PermissionOptionKind::RejectOnce,
                    ))
                    .unwrap_or_else(|_| panic!("{name}: the tool waits for the answer"));
                assert!(task.await.is_err(), "{name}: a denial fails the tool");
                assert_eq!(*browser.calls.lock(), Vec::new(), "{name}");
            } else {
                assert!(task.await.is_ok(), "{name}");
                receiver.expect_no_authorization();
                assert_eq!(browser.calls.lock().drain(..).count(), 1, "{name}");
            }
        }
    }

    #[gpui::test]
    async fn turning_ai_control_off_while_asking_refuses_the_call(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        let (event_stream, mut receiver) = ToolCallEventStream::test();

        let task = run(
            BrowserEvalTool::new(project),
            json!({ "expression": "document.title" }),
            event_stream,
            cx,
        );
        let authorization = receiver.expect_authorization().await;
        browser.enabled.store(false, Ordering::SeqCst);
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("the tool waits for the answer");
        let error = task.await.expect_err("the tool refuses");
        assert_eq!(error.text, browser_control::AI_CONTROL_OFF_MESSAGE);
        assert_eq!(*browser.calls.lock(), Vec::new());
    }

    /// "Always allow" choices that name a pattern write it into the user's settings. Only the
    /// tools that load a URL get one, for its host as `fetch` does; text typed into a page, which
    /// may be a password, must never become a pattern.
    #[test]
    fn only_urls_become_always_allow_patterns() {
        let patterns = |name: &str, input: &str| {
            let options = ToolPermissionContext::new(name, vec![input.to_string()])
                .build_permission_options();
            let acp_thread::PermissionOptions::Dropdown(choices) = options else {
                panic!("{name}: a dropdown of choices");
            };
            for choice in &choices {
                assert!(
                    !choice.allow.name.contains("hunter2") && !choice.deny.name.contains("hunter2"),
                    "{name}: {choice:?}"
                );
            }
            choices
                .into_iter()
                .flat_map(|choice| choice.sub_patterns)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            patterns("browser_navigate", "https://example.com/app"),
            vec!["^https?://example\\.com".to_string()]
        );
        assert_eq!(
            patterns("browser_open_tab", "http://localhost:3000/"),
            vec!["^https?://localhost".to_string()]
        );
        assert_eq!(
            patterns("browser_open_tab", "about:blank"),
            Vec::<String>::new()
        );
        for back_forward_reload in ["back", "forward", "reload"] {
            assert_eq!(
                patterns("browser_navigate", back_forward_reload),
                Vec::<String>::new()
            );
        }
        for name in BROWSER_ACTING_TOOL_NAMES {
            if *name == "browser_navigate" || *name == "browser_open_tab" {
                continue;
            }
            assert_eq!(patterns(name, "hunter2"), Vec::<String>::new(), "{name}");
            assert_eq!(
                patterns(name, "https://example.com/hunter2"),
                Vec::<String>::new(),
                "{name}"
            );
        }
    }

    #[gpui::test]
    async fn turning_ai_control_off_refuses_a_call_already_handed_out(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;
        browser.enabled.store(false, Ordering::SeqCst);
        let (event_stream, mut receiver) = ToolCallEventStream::test();

        let error = run(BrowserTabsTool::new(project), json!({}), event_stream, cx)
            .await
            .expect_err("the tool refuses");
        assert!(error.text.contains("browser.ai_control"), "{}", error.text);
        receiver.expect_no_authorization();
        assert_eq!(*browser.calls.lock(), Vec::new());
    }

    #[gpui::test]
    async fn bad_arguments_and_browser_failures_are_tool_errors(cx: &mut TestAppContext) {
        let browser = init_test(cx);
        let project = test_project(cx).await;

        let (event_stream, _receiver) = ToolCallEventStream::test();
        let error = run(
            BrowserClickTool::new(project.clone()),
            json!({ "selector": "a" }),
            event_stream,
            cx,
        )
        .await
        .expect_err("unknown arguments are refused");
        assert!(error.text.contains("unknown argument"), "{}", error.text);
        assert_eq!(*browser.calls.lock(), Vec::new());

        *browser.reply.lock() = Some(Err("no AI tab is open".to_string()));
        let (event_stream, _receiver) = ToolCallEventStream::test();
        let error = run(
            BrowserPageTextTool::new(project),
            json!({}),
            event_stream,
            cx,
        )
        .await
        .expect_err("the browser's failure is the tool's");
        assert_eq!(error.text, "no AI tab is open");
    }

    #[test]
    fn titles_describe_the_call() {
        assert_eq!(
            browser_tool_title("browser_click", json!({ "ref": "e7" })),
            "Click `e7`"
        );
        assert_eq!(
            browser_tool_title("browser_click", json!({ "x": 10, "y": 20.5 })),
            "Click (10, 20.5)"
        );
        assert_eq!(
            browser_tool_title("browser_click", json!({})),
            "Click on the page"
        );
        assert_eq!(
            browser_tool_title("browser_eval", json!({ "expression": "document.title" })),
            "Run `document.title` in the page"
        );
        let long = "a".repeat(MAX_TITLE_TEXT_CHARS + 10);
        assert_eq!(
            browser_tool_title("browser_type", json!({ "text": long })),
            format!("Type `{}…`", "a".repeat(MAX_TITLE_TEXT_CHARS))
        );
        for name in BROWSER_TOOL_NAMES {
            assert_ne!(generic_title(name), i18n::t!("Use the browser"), "{name}");
        }
    }
}
