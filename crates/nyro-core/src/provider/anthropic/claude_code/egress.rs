//! Final-request shaping for the Claude Code OAuth channel.
//!
//! Called once per upstream target from the dispatcher, after every request
//! path (native passthrough, IR encode, raw-wire compat) has produced its body
//! and merged headers. It decides whether the downstream caller is genuine
//! Claude Code and then:
//!
//! * **native caller** — keeps the caller's body and measured identity
//!   headers (the runtime binding's static UA no longer clobbers them), adds
//!   the credential-scoped OAuth betas, rebinds `metadata.user_id` to the
//!   selected account, and restores the `cch` billing attestation;
//! * **any other caller** — cloaks the body into the Claude Code CLI shape
//!   (`cloak`), aliases client tool names to MCP names (`tool_alias`) and
//!   sends the full CLI header profile with a per-request beta list.
//!
//! The returned [`ClaudeCodeWireHook`] signs the final serialized bytes and
//! restores tool names in the response at the `ProxyClient` boundary.

use std::sync::Arc;

use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::auth::types::StoredCredential;
use crate::db::models::Provider;
use crate::proxy::client::EgressHook;

use super::profile::{self, header_str};
use super::tool_alias::{self, ToolAliasMap};
use super::{cch, cloak, identity};

/// Whether the provider row is the Anthropic Claude Code OAuth channel.
pub(crate) fn is_claude_code_channel(provider: &Provider) -> bool {
    provider
        .vendor
        .as_deref()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("anthropic"))
        && provider
            .channel
            .as_deref()
            .is_some_and(|c| c.trim().eq_ignore_ascii_case("claude-code"))
}

pub(crate) struct EgressInput<'a> {
    pub provider: &'a Provider,
    pub credential: Option<&'a StoredCredential>,
    /// Raw downstream request headers (before any forwarding filter).
    pub incoming: &'a HeaderMap,
    /// Stable downstream caller scope (gateway API-key id) for MCP aliases.
    pub caller_scope: &'a str,
    /// Local calendar date for the `currentDate` reminder.
    pub date: &'a str,
}

/// Outcome of [`prepare`], attached to the transport as an [`EgressHook`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeCodeWireHook {
    aliases: ToolAliasMap,
    /// Whether the caller was detected as genuine Claude Code.
    pub native: bool,
}

impl EgressHook for ClaudeCodeWireHook {
    fn finalize_request_body(&self, body: Bytes) -> Bytes {
        match cch::sign_body(&body) {
            Ok(signed) => Bytes::from(signed),
            Err(error) => {
                tracing::warn!(%error, "claude-code: cch signing skipped");
                body
            }
        }
    }

    fn rewrites_response(&self) -> bool {
        !self.aliases.is_empty()
    }

    fn rewrite_response_line(&self, line: &str) -> Option<String> {
        tool_alias::restore_sse_line(line, &self.aliases)
    }

    fn rewrite_response_json(&self, body: &[u8]) -> Option<Vec<u8>> {
        tool_alias::restore_response_json(body, &self.aliases)
    }
}

impl ClaudeCodeWireHook {
    pub(crate) fn into_hook(self) -> Arc<dyn EgressHook> {
        Arc::new(self)
    }
}

/// A request the channel refuses before sending (always the caller's fault).
#[derive(Debug, thiserror::Error)]
#[error("invalid_request_error: {0}")]
pub(crate) struct InvalidRequest(pub String);

fn set(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn is_first_party(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.anthropic.com")
            && url.port_or_known_default() == Some(443)
    })
}

/// Claude Code always calls `…/v1/messages?beta=true`.
fn with_beta_query(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.query_pairs().any(|(key, _)| key == "beta") {
        return url.to_string();
    }
    parsed.query_pairs_mut().append_pair("beta", "true");
    parsed.to_string()
}

fn incoming_betas(incoming: &HeaderMap) -> Vec<String> {
    incoming
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(profile::split_betas)
        .collect()
}

/// Caller headers a confirmed native client owns (`copyClaudeCallerFingerprintHeaders`).
fn is_native_identity_header(name: &str) -> bool {
    matches!(
        name,
        "user-agent"
            | "x-app"
            | "x-client-request-id"
            | "x-client-app"
            | "x-anthropic-additional-protection"
    ) || name.starts_with("x-stainless-")
        || name.starts_with("x-claude-code-")
        || name.starts_with("x-claude-remote-")
        || (name.starts_with("anthropic-") && name != "anthropic-beta")
}

/// Agent / environment headers forwarded for every caller when present.
const AGENT_HEADERS: &[&str] = &[
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "x-claude-remote-container-id",
    "x-claude-remote-session-id",
    "x-client-app",
    "x-anthropic-additional-protection",
];

fn cli_profile_headers(headers: &mut HeaderMap, count_tokens: bool, only_missing: bool) {
    let mut put = |name: &'static str, value: &str| {
        if !only_missing || !headers.contains_key(name) {
            set(headers, name, value);
        }
    };
    put("anthropic-version", "2023-06-01");
    put("anthropic-dangerous-direct-browser-access", "true");
    put("x-app", "cli");
    put("user-agent", profile::CLI_USER_AGENT);
    put("x-stainless-lang", "js");
    put(
        "x-stainless-package-version",
        profile::STAINLESS_PACKAGE_VERSION,
    );
    put("x-stainless-os", profile::STAINLESS_OS);
    put("x-stainless-arch", profile::STAINLESS_ARCH);
    put("x-stainless-runtime", "node");
    put(
        "x-stainless-runtime-version",
        profile::STAINLESS_RUNTIME_VERSION,
    );
    put("x-stainless-retry-count", "0");
    if !count_tokens {
        put("x-stainless-timeout", profile::STAINLESS_TIMEOUT);
    }
}

/// count_tokens keeps Claude Code's measured shape: model, messages, tools.
fn shape_count_tokens(body: &mut Value, native: bool) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if native {
        for key in ["metadata", "context_management", "diagnostics"] {
            obj.remove(key);
        }
        return;
    }
    let system = obj.remove("system");
    obj.retain(|key, _| matches!(key.as_str(), "model" | "messages" | "tools"));
    // Count the caller's system prompt where the Messages path will put it,
    // never in the system slot (`relocateClaudeSystemPromptForCountTokens`).
    cloak::relocate_caller_system_for_count(body, system.as_ref());
}

/// Shape `headers`, `url` and `body` for the Claude Code OAuth upstream.
pub(crate) fn prepare(
    input: &EgressInput<'_>,
    url: &mut String,
    headers: &mut HeaderMap,
    body: &mut Value,
) -> Result<ClaudeCodeWireHook, InvalidRequest> {
    if !body.is_object() {
        return Ok(ClaudeCodeWireHook::default());
    }
    let count_tokens = reqwest::Url::parse(url).is_ok_and(|u| u.path().ends_with("/count_tokens"));
    let first_party = is_first_party(url);
    let detection = profile::detect_native(input.incoming, body, count_tokens);
    let native = detection.confirmed;

    let auth = headers.get(AUTHORIZATION).cloned();
    let api_key = headers.get("x-api-key").cloned();
    let oauth = auth
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("bearer "));

    let account = identity::account_uuid(input.credential, &input.provider.id);
    let device = identity::device_id(&format!("{}\x00{account}", input.provider.id));
    let session = identity::session_uuid(input.incoming, body, native);

    let body_betas = cloak::take_body_betas(body);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("prompt_cache_options");
    }
    let subagent = profile::is_subagent(input.incoming, body);

    let mut aliases = ToolAliasMap::default();
    if count_tokens {
        shape_count_tokens(body, native);
        if !native {
            aliases = tool_alias::alias_request_tools(body, &alias_secret(input));
        }
    } else if native {
        cloak::normalize_sampling(body, true);
        cloak::disable_thinking_if_tool_choice_forced(body);
        cloak::enforce_cache_control_limit(body);
        cloak::normalize_cache_ttl_order(body);
        cloak::sanitize_web_search_domains(body);
        cloak::apply_preserved_thinking_binding(body);
        identity::apply_user_id(body, &device, &account, &session, true);
        let entrypoint = detection.entrypoint.as_deref().unwrap_or("cli");
        cloak::ensure_native_billing(body, entrypoint, subagent);
    } else {
        let ctx = cloak::CloakContext {
            date: input.date,
            subagent,
            first_party,
        };
        cloak::cloak(body, &ctx).map_err(InvalidRequest)?;
        aliases = tool_alias::alias_request_tools(body, &alias_secret(input));
        identity::apply_user_id(body, &device, &account, &session, false);
    }

    // ── Headers ──────────────────────────────────────────────────────────────
    let mut caller_betas = incoming_betas(input.incoming);
    if !native {
        // Capability requests added on the way here (e.g. the raw-wire
        // compat layer selecting `context-1m`) count as caller requests too.
        for beta in incoming_betas(headers) {
            if !caller_betas.contains(&beta) {
                caller_betas.push(beta);
            }
        }
    }
    let mut out = HeaderMap::new();
    if native {
        for (name, value) in input.incoming {
            if is_native_identity_header(name.as_str()) {
                out.insert(name.clone(), value.clone());
            }
        }
        let current =
            header_str(&out, "user-agent").is_some_and(profile::caller_user_agent_is_current);
        if !current {
            // An older (or unparseable) client would advertise a software
            // profile this gateway has not measured; send the baseline.
            cli_profile_headers(&mut out, count_tokens, false);
        }
        cli_profile_headers(&mut out, count_tokens, true);
    } else {
        cli_profile_headers(&mut out, count_tokens, false);
        for name in AGENT_HEADERS {
            if let Some(value) = input.incoming.get(*name) {
                out.insert(HeaderName::from_static(name), value.clone());
            }
        }
    }

    let betas = if count_tokens {
        count_tokens_betas(native, &caller_betas, oauth, body)
    } else if native {
        let mut list = caller_betas.clone();
        if !first_party {
            list.extend(body_betas.iter().cloned());
        }
        profile::native_betas(body, &list, oauth, input.incoming)
    } else {
        profile::cloaked_betas(
            body,
            &caller_betas,
            &body_betas,
            oauth,
            first_party,
            input.incoming,
        )
    };
    if !betas.is_empty() {
        set(&mut out, "anthropic-beta", &betas);
    }
    set(&mut out, identity::SESSION_HEADER, &session);
    if first_party && !out.contains_key("x-client-request-id") {
        set(
            &mut out,
            "x-client-request-id",
            &uuid::Uuid::new_v4().hyphenated().to_string(),
        );
    }
    set(&mut out, "content-type", "application/json");
    set(&mut out, "accept", "application/json");
    if let Some(auth) = auth {
        out.insert(AUTHORIZATION, auth);
    }
    if let Some(api_key) = api_key {
        out.insert(HeaderName::from_static("x-api-key"), api_key);
    }
    *headers = out;
    *url = with_beta_query(url);

    Ok(ClaudeCodeWireHook { aliases, native })
}

fn count_tokens_betas(native: bool, caller: &[String], oauth: bool, body: &Value) -> String {
    let has_advisor = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|t| {
                t.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|ty| ty.trim().to_ascii_lowercase().starts_with("advisor_"))
            })
        });
    let mut list: Vec<String> = if native && !caller.is_empty() {
        caller.to_vec()
    } else {
        vec![
            profile::CLAUDE_CODE_BETA.to_string(),
            "interleaved-thinking-2025-05-14".to_string(),
            "context-management-2025-06-27".to_string(),
            "token-counting-2024-11-01".to_string(),
        ]
    };
    if oauth && !list.iter().any(|b| b == profile::OAUTH_BETA) {
        let at = usize::from(list.first().map(String::as_str) == Some(profile::CLAUDE_CODE_BETA));
        list.insert(at, profile::OAUTH_BETA.to_string());
    }
    if has_advisor && !list.iter().any(|b| b == "advisor-tool-2026-03-01") {
        list.push("advisor-tool-2026-03-01".to_string());
    }
    list.join(",")
}

/// Caller-scoped MCP alias secret: stable per (provider, downstream key), so
/// names survive token refresh and stay prompt-cache stable per caller.
fn alias_secret(input: &EgressInput<'_>) -> String {
    format!(
        "nyro-claude-code-mcp:v1\x00{}\x00{}",
        input.provider.id, input.caller_scope
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider() -> Provider {
        Provider {
            keys: Vec::new(),
            id: "p-1".into(),
            name: "cc".into(),
            vendor: Some("anthropic".into()),
            protocol: "anthropic-messages".into(),
            base_url: "https://api.anthropic.com".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("anthropic".into()),
            channel: Some("claude-code".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn credential() -> StoredCredential {
        StoredCredential {
            access_token: Some("sk-ant-oat01-test".into()),
            meta: json!({"account": {"uuid": "1b4e28ba-2fa1-11d2-883f-0016d3cca427"}}),
            ..Default::default()
        }
    }

    fn outbound_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-ant-oat01-test"),
        );
        headers.insert(
            "user-agent",
            HeaderValue::from_static("claude-cli/1.0.98 (external, cli)"),
        );
        headers.insert("origin", HeaderValue::from_static("https://claude.ai"));
        headers.insert("x-stainless-lang", HeaderValue::from_static("python"));
        headers.insert("openai-organization", HeaderValue::from_static("org-leak"));
        headers
    }

    fn run(incoming: &HeaderMap, body: &mut Value) -> (String, HeaderMap, ClaudeCodeWireHook) {
        let provider = provider();
        let credential = credential();
        let input = EgressInput {
            provider: &provider,
            credential: Some(&credential),
            incoming,
            caller_scope: "key-1",
            date: "2026-10-28",
        };
        let mut url = "https://api.anthropic.com/v1/messages".to_string();
        let mut headers = outbound_headers();
        let hook = prepare(&input, &mut url, &mut headers, body).unwrap();
        (url, headers, hook)
    }

    #[test]
    fn third_party_caller_gets_full_cli_profile() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            "user-agent",
            HeaderValue::from_static("OpenAI/Python 1.40.0"),
        );
        incoming.insert("x-stainless-lang", HeaderValue::from_static("python"));
        let mut body = json!({
            "model": "claude-opus-4-7",
            "system": "You are OpenCode.",
            "max_tokens": 4096,
            "tools": [{"name": "bash", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "list files"}]
        });
        let (url, headers, hook) = run(&incoming, &mut body);

        assert_eq!(url, "https://api.anthropic.com/v1/messages?beta=true");
        assert!(!hook.native);
        assert_eq!(headers["user-agent"], profile::CLI_USER_AGENT);
        assert_eq!(headers["x-app"], "cli");
        assert_eq!(headers["x-stainless-lang"], "js");
        assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-test");
        assert!(
            headers.get("origin").is_none(),
            "binding-only headers are not sent"
        );
        assert!(headers.get("openai-organization").is_none());
        let betas = headers["anthropic-beta"].to_str().unwrap();
        assert!(
            betas.starts_with("claude-code-20250219,oauth-2025-04-20,"),
            "{betas}"
        );
        let session = headers[identity::SESSION_HEADER]
            .to_str()
            .unwrap()
            .to_string();

        // Body: cloaked system, aliased tool, credential identity.
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("x-anthropic-billing-header:")
        );
        assert_eq!(body["system"][1]["text"], cloak::CLAUDE_CODE_IDENTITY);
        let tool = body["tools"][0]["name"].as_str().unwrap();
        assert!(
            tool.starts_with("mcp__") && tool.ends_with("_bash"),
            "{tool}"
        );
        let user_id: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            user_id["account_uuid"],
            "1b4e28ba-2fa1-11d2-883f-0016d3cca427"
        );
        assert_eq!(user_id["session_id"], session);

        // The hook signs the final bytes and restores the tool name.
        let signed = hook.finalize_request_body(Bytes::from(serde_json::to_vec(&body).unwrap()));
        let signed: Value = serde_json::from_slice(&signed).unwrap();
        let billing = signed["system"][0]["text"].as_str().unwrap();
        assert!(!billing.contains("cch=00000;"), "{billing}");
        let line = format!(
            r#"data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"t","name":"{tool}","input":{{}}}}}}"#
        );
        assert!(
            hook.rewrite_response_line(&line)
                .unwrap()
                .contains(r#""name":"bash""#)
        );
    }

    #[test]
    fn native_claude_code_keeps_its_identity_and_body() {
        let session = "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d";
        let mut incoming = HeaderMap::new();
        incoming.insert(
            "user-agent",
            HeaderValue::from_static("claude-cli/2.1.290 (external, cli)"),
        );
        incoming.insert("x-app", HeaderValue::from_static("cli"));
        incoming.insert(
            "anthropic-beta",
            HeaderValue::from_static(
                "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14",
            ),
        );
        incoming.insert(
            "x-stainless-package-version",
            HeaderValue::from_static("0.113.0"),
        );
        incoming.insert(identity::SESSION_HEADER, HeaderValue::from_static(session));
        let user_id =
            json!({"device_id": "c".repeat(64), "account_uuid": "", "session_id": session})
                .to_string();
        let mut body = json!({
            "model": "claude-opus-4-7",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.290.abc; cc_entrypoint=cli;"},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Long native prompt"}
            ],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "metadata": {"user_id": user_id},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (_, headers, hook) = run(&incoming, &mut body);

        assert!(hook.native);
        assert_eq!(headers["user-agent"], "claude-cli/2.1.290 (external, cli)");
        assert_eq!(headers["x-stainless-package-version"], "0.113.0");
        assert_eq!(headers[identity::SESSION_HEADER], session);
        assert_eq!(
            headers["anthropic-beta"],
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,extended-cache-ttl-2025-04-11"
        );
        // Body stays native: system untouched apart from the cch placeholder.
        assert_eq!(body["system"][2]["text"], "Long native prompt");
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert!(
            body["system"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with("cc_entrypoint=cli; cch=00000;")
        );
        let rebound: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            rebound["account_uuid"],
            "1b4e28ba-2fa1-11d2-883f-0016d3cca427"
        );
        assert_eq!(rebound["session_id"], session);
        assert_ne!(rebound["device_id"], "c".repeat(64));
        assert!(!hook.rewrites_response());
    }

    #[test]
    fn non_text_system_block_is_an_invalid_request() {
        let provider = provider();
        let incoming = HeaderMap::new();
        let input = EgressInput {
            provider: &provider,
            credential: None,
            incoming: &incoming,
            caller_scope: "",
            date: "2026-10-28",
        };
        let mut url = "https://api.anthropic.com/v1/messages".to_string();
        let mut headers = outbound_headers();
        let mut body =
            json!({"system": [{"type": "image"}], "messages": [{"role": "user", "content": "x"}]});
        assert!(prepare(&input, &mut url, &mut headers, &mut body).is_err());
    }
}
