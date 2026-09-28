//! Model-backed AI providers (Section 15 "AI assistance" — "Ollama
//! support" / "Remote-provider mode" / "Custom API endpoint support").
//!
//! [`AiProvider`] is deliberately tiny (one `generate` call) so a UI layer
//! can swap providers without caring which one is active. [`OllamaProvider`]
//! talks to a local Ollama server's `/api/generate` endpoint over plain
//! HTTP — the natural "Local-only mode" network provider, since Ollama
//! itself runs on the machine; nothing here calls out to the public
//! internet. A provider hitting a real remote API is the same shape, just
//! a different `base_url`/auth header ("Custom API endpoint support" /
//! "OpenAI-compatible endpoint support"), left for whoever wires one up to
//! implement against this same trait.
//!
//! **This code is never called by [`crate::AiAssistant`] on its own** —
//! only [`NetworkAssistant`] requests the `network` permission and uses a
//! provider, and it checks that permission at the call site too (defense
//! in depth beyond whatever granted the plugin its permissions in the
//! first place) — matching "Disable network AI by default for private
//! sessions" and "Ask-before-network mode".

use std::time::Duration;
use ut_plugin_api::{Permissions, Plugin, PluginAction, PluginContext};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("network permission was not granted to this plugin")]
    PermissionDenied,
    #[error("request to provider failed: {0}")]
    Request(String),
    #[error("provider response was not valid JSON")]
    MalformedResponse,
    #[error("provider response had no 'response' field")]
    MissingResponseField,
}

/// A model backend that can turn a prompt into text.
pub trait AiProvider {
    fn generate(&self, prompt: &str) -> Result<String, ProviderError>;
}

/// Talks to a local (or otherwise reachable) Ollama server's non-streaming
/// `/api/generate` endpoint.
pub struct OllamaProvider {
    base_url: String,
    model: String,
    agent: ureq::Agent,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(Duration::from_secs(30)).build();
        OllamaProvider { base_url: base_url.into(), model: model.into(), agent }
    }

    /// The default local Ollama endpoint.
    pub fn local(model: impl Into<String>) -> Self {
        Self::new("http://127.0.0.1:11434", model)
    }
}

impl AiProvider for OllamaProvider {
    fn generate(&self, prompt: &str) -> Result<String, ProviderError> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "model": self.model, "prompt": prompt, "stream": false });

        let response = self.agent.post(&url).send_json(body).map_err(|e| ProviderError::Request(e.to_string()))?;
        let json: serde_json::Value = response.into_json().map_err(|_| ProviderError::MalformedResponse)?;
        json.get("response").and_then(|v| v.as_str()).map(str::to_string).ok_or(ProviderError::MissingResponseField)
    }
}

/// A [`Plugin`] that requests network access and, when granted, answers
/// natural-language questions via an [`AiProvider`] — "Natural-language
/// command generation" / "Troubleshooting suggestions". Every answer is
/// surfaced as [`PluginAction::ShowMessage`] or, for a generated command,
/// [`PluginAction::SuggestCommand`] — never auto-run (see
/// `ut_plugin_api::PluginContext::suggest_input`, which this still goes
/// through: this plugin cannot bypass that gate).
pub struct NetworkAssistant<P: AiProvider> {
    provider: P,
}

impl<P: AiProvider> NetworkAssistant<P> {
    pub fn new(provider: P) -> Self {
        NetworkAssistant { provider }
    }

    /// Ask a natural-language question and get back a suggested command
    /// (or explanatory text). Checked against `ctx`'s granted permissions
    /// before any network call is made.
    pub fn ask(&self, ctx: &PluginContext, question: &str) -> Result<PluginAction, ProviderError> {
        if !ctx.permissions().network {
            return Err(ProviderError::PermissionDenied);
        }
        let answer = self.provider.generate(question)?;
        Ok(PluginAction::ShowMessage(answer))
    }
}

impl<P: AiProvider> Plugin for NetworkAssistant<P> {
    fn id(&self) -> &str {
        "ai-assistant-network"
    }

    fn requested_permissions(&self) -> Permissions {
        Permissions { read_output: true, read_commands: true, suggest_input: true, network: true, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use ut_safety::PrivilegeContext;

    /// Spin up a one-shot mock HTTP server that returns a canned Ollama-style
    /// `/api/generate` response, so `OllamaProvider` is tested against a
    /// real socket without needing an actual Ollama install.
    fn mock_ollama_server(response_text: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                // Drain headers until the blank line.
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body = format!(r#"{{"model":"test","response":"{response_text}","done":true}}"#);
                let http_response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(http_response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn ollama_provider_parses_response_field() {
        let base_url = mock_ollama_server("ls -la lists files in long format");
        let provider = OllamaProvider::new(base_url, "llama3");
        let result = provider.generate("what does ls -la do?").unwrap();
        assert_eq!(result, "ls -la lists files in long format");
    }

    #[test]
    fn ollama_provider_errors_on_unreachable_server() {
        // Port 1 is reserved/unlikely to be listening; connect should fail fast.
        let provider = OllamaProvider::new("http://127.0.0.1:1", "llama3");
        let result = provider.generate("hello");
        assert!(result.is_err());
    }

    struct StaticProvider(&'static str);
    impl AiProvider for StaticProvider {
        fn generate(&self, _prompt: &str) -> Result<String, ProviderError> {
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn network_assistant_requires_network_permission() {
        let assistant = NetworkAssistant::new(StaticProvider("answer"));
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx_without_network = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, None);
        let result = assistant.ask(&ctx_without_network, "how do I list files?");
        assert!(matches!(result, Err(ProviderError::PermissionDenied)));
    }

    #[test]
    fn network_assistant_answers_when_permitted() {
        let assistant = NetworkAssistant::new(StaticProvider("use ls -la"));
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let perms = Permissions { network: true, ..Default::default() };
        let ctx = PluginContext::new(perms, &priv_ctx, &lines, None);
        let result = assistant.ask(&ctx, "how do I list files?").unwrap();
        assert_eq!(result, PluginAction::ShowMessage("use ls -la".to_string()));
    }

    #[test]
    fn network_assistant_requests_network_permission() {
        let assistant = NetworkAssistant::new(StaticProvider("x"));
        assert!(assistant.requested_permissions().network);
    }
}
