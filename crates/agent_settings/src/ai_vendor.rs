//! ADR-F94: which company's AI a language model provider or an external agent
//! is, and how its user signs in to it. The agent panel's "Choose AI" menu and
//! the settings window's "AI Connections" page both group by this, so the two
//! never file the same thing under different vendors.

/// The vendor an AI belongs to, in the order the two lists show them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AiVendor {
    Anthropic,
    OpenAi,
    Google,
    /// Models served from this machine or the user's own network.
    Local,
    Other,
}

impl AiVendor {
    pub const ALL: [AiVendor; 5] = [
        AiVendor::Anthropic,
        AiVendor::OpenAi,
        AiVendor::Google,
        AiVendor::Local,
        AiVendor::Other,
    ];
}

/// How the user signs in to a language model provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderSignIn {
    ApiKey,
    /// The vendor's own subscription sign-in (ChatGPT, for instance).
    Subscription,
    /// An account with a service that resells models (Zed, GitHub Copilot).
    Account,
    /// Nothing to sign in to: a server on this machine or network.
    Local,
}

pub const ANTHROPIC_AGENT_ID: &str = "claude-acp";
pub const OPENAI_AGENT_ID: &str = "codex-acp";
pub const GOOGLE_AGENT_ID: &str = "gemini";

pub fn vendor_of_language_model_provider(provider_id: &str) -> AiVendor {
    match provider_id {
        "anthropic" => AiVendor::Anthropic,
        "openai" | "openai-subscribed" => AiVendor::OpenAi,
        "google" => AiVendor::Google,
        "ollama" | "lmstudio" | "llama.cpp" => AiVendor::Local,
        _ => AiVendor::Other,
    }
}

pub fn sign_in_of_language_model_provider(provider_id: &str) -> ProviderSignIn {
    match vendor_of_language_model_provider(provider_id) {
        AiVendor::Local => ProviderSignIn::Local,
        _ => match provider_id {
            "openai-subscribed" => ProviderSignIn::Subscription,
            "zed.dev" | "copilot_chat" => ProviderSignIn::Account,
            _ => ProviderSignIn::ApiKey,
        },
    }
}

/// Only the ACP registry's own entries are filed under a vendor: a command
/// the user configured under one of those names could run anything, which is
/// the rule `project::agent_sandbox::SandboxedAgentProfile::for_agent` follows.
pub fn vendor_of_external_agent(agent_id: &str, from_registry: bool) -> AiVendor {
    if !from_registry {
        return AiVendor::Other;
    }
    match agent_id {
        ANTHROPIC_AGENT_ID => AiVendor::Anthropic,
        OPENAI_AGENT_ID => AiVendor::OpenAi,
        GOOGLE_AGENT_ID => AiVendor::Google,
        _ => AiVendor::Other,
    }
}

/// The providers offered even before they are set up, so that a vendor's
/// pay-as-you-go route can be found from its section. The others appear once
/// they work, to keep the list short.
pub fn is_language_model_provider_offered_before_setup(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "anthropic" | "openai" | "openai-subscribed" | "google"
    )
}

/// ADR-F10: whether a language model provider may be used without asking for
/// the external-AI consent first. The rule (which hosts count as external,
/// where the consent is kept) lives with the consent screens in `settings_ui`,
/// which registers it here; the agent panel cannot depend on that crate.
#[derive(Clone, Copy)]
pub struct LanguageModelProviderConsentCheck(pub fn(&str, &gpui::App) -> bool);

impl gpui::Global for LanguageModelProviderConsentCheck {}

/// `false` while nothing has registered the rule: a thread on a provider that
/// cannot be shown to be consented goes through the setup window instead.
pub fn language_model_provider_is_consented(provider_id: &str, cx: &gpui::App) -> bool {
    cx.try_global::<LanguageModelProviderConsentCheck>()
        .is_some_and(|check| (check.0)(provider_id, cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_filed_under_their_vendor() {
        assert_eq!(
            vendor_of_language_model_provider("anthropic"),
            AiVendor::Anthropic
        );
        assert_eq!(
            vendor_of_language_model_provider("openai"),
            AiVendor::OpenAi
        );
        assert_eq!(
            vendor_of_language_model_provider("openai-subscribed"),
            AiVendor::OpenAi
        );
        assert_eq!(
            vendor_of_language_model_provider("google"),
            AiVendor::Google
        );
        for local in ["ollama", "lmstudio", "llama.cpp"] {
            assert_eq!(vendor_of_language_model_provider(local), AiVendor::Local);
        }
        for other in ["zed.dev", "copilot_chat", "openrouter", "my-compatible"] {
            assert_eq!(vendor_of_language_model_provider(other), AiVendor::Other);
        }
    }

    #[test]
    fn provider_sign_in_follows_what_the_provider_asks_for() {
        assert_eq!(
            sign_in_of_language_model_provider("anthropic"),
            ProviderSignIn::ApiKey
        );
        assert_eq!(
            sign_in_of_language_model_provider("openai-subscribed"),
            ProviderSignIn::Subscription
        );
        assert_eq!(
            sign_in_of_language_model_provider("zed.dev"),
            ProviderSignIn::Account
        );
        assert_eq!(
            sign_in_of_language_model_provider("copilot_chat"),
            ProviderSignIn::Account
        );
        assert_eq!(
            sign_in_of_language_model_provider("ollama"),
            ProviderSignIn::Local
        );
        assert_eq!(
            sign_in_of_language_model_provider("openrouter"),
            ProviderSignIn::ApiKey
        );
    }

    #[test]
    fn only_registry_agents_take_a_vendor() {
        assert_eq!(
            vendor_of_external_agent("claude-acp", true),
            AiVendor::Anthropic
        );
        assert_eq!(
            vendor_of_external_agent("codex-acp", true),
            AiVendor::OpenAi
        );
        assert_eq!(vendor_of_external_agent("gemini", true), AiVendor::Google);
        assert_eq!(vendor_of_external_agent("opencode", true), AiVendor::Other);
        assert_eq!(
            vendor_of_external_agent("claude-acp", false),
            AiVendor::Other
        );
    }
}
