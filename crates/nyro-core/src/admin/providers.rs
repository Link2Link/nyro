use super::*;

#[derive(Debug)]
struct NormalizedProtocolConfig {
    mode: String,
    default_protocol: String,
    base_url: String,
    api_key: String,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
}

/// Per-model result of the "send hi" probe over a provider's model list.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeResult {
    pub model: String,
    pub success: bool,
    pub error: Option<String>,
    pub latency_ms: u64,
    /// Canonical protocol endpoint id used for the probe (e.g.
    /// `openai-compatible/chat-completions/v1`).
    pub protocol: String,
    /// Assistant text received for the "hi" probe (success only).
    /// The value "[completed]" means the upstream completed without displayable text.
    pub reply: Option<String>,
}

/// Which protocol/base_url the probe ran through (reported once per run).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeMeta {
    pub protocol: String,
    pub base_url: String,
}

/// Full probe response: shared run metadata plus per-model results.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelProbeOutcome {
    pub meta: ProviderModelProbeMeta,
    pub results: Vec<ProviderModelProbeResult>,
}

#[allow(clippy::too_many_arguments)]
fn build_model_probe_request(
    suite: crate::protocol::ids::Protocol,
    base_url: &str,
    api_key: &str,
    auth_scheme: &str,
    runtime_headers: &HeaderMap,
    model: &str,
    fast_mode: bool,
    channel: Option<&str>,
    is_codex_oauth: bool,
    antigravity_project: Option<&str>,
) -> anyhow::Result<(String, HeaderMap, Value)> {
    // Reasoning models (e.g. glm-5.3) burn the whole completion budget on
    // thinking before emitting visible text. 1024 leaves room for both.
    let (path, mut body) = match suite {
        crate::protocol::ids::Protocol::OpenAICompatible => (
            "/v1/chat/completions",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1024,
                "stream": false,
            }),
        ),
        crate::protocol::ids::Protocol::OpenAIResponses => (
            "/v1/responses",
            if is_codex_oauth {
                // ChatGPT's internal Codex endpoint requires canonical
                // Responses input items and returns SSE even for admin probes.
                serde_json::json!({
                    "model": model,
                    "input": [{
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hi"}],
                    }],
                    "instructions": "You are a helpful assistant.",
                    "store": false,
                    "stream": true,
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "input": "hi",
                    "max_output_tokens": 1024,
                    "stream": false,
                })
            },
        ),
        crate::protocol::ids::Protocol::AnthropicMessages => (
            "/v1/messages",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1024,
            }),
        ),
        crate::protocol::ids::Protocol::GoogleGemini => {
            // Gemini wire format is fully self-contained: return early with a
            // complete (url, headers, body) so the generic auth-scheme logic
            // below (bearer/x-api-key) never mis-attaches.
            return build_gemini_model_probe_request(
                base_url,
                api_key,
                runtime_headers,
                model,
                channel,
                antigravity_project,
            );
        }
    };

    crate::provider::common::pipeline::maybe_inject_openai_fast_mode_for_protocol(
        &mut body, fast_mode, channel, suite,
    );

    let path = if is_codex_oauth && path == "/v1/responses" {
        "/responses"
    } else {
        path
    };
    let mut url = crate::provider::common::openai::openai_build_url(base_url, path);
    let mut headers = HeaderMap::new();
    match auth_scheme {
        "bearer" => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))?,
            );
        }
        "x-api-key" => {
            headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
            if suite == crate::protocol::ids::Protocol::AnthropicMessages {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        "query" => {
            let separator = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{separator}key={api_key}");
        }
        "none" => {}
        other => anyhow::bail!("unsupported auth scheme: {other}"),
    }
    // OAuth runtime identity is provider-owned and authoritative, matching
    // the dispatcher precedence (default auth < RuntimeBinding headers).
    headers.extend(runtime_headers.clone());
    Ok((url, headers, body))
}

/// Build a minimal "hi" probe for a google-gemini provider.
///
/// Both shapes stream (`streamGenerateContent?alt=sse`) and let the SSE
/// reply extractor aggregate — mirroring sub2api's account test service,
/// because the Code Assist surface's non-stream action can return empty
/// bodies.
///
/// * `google/antigravity` (Google AI Pro OAuth): Bearer + Antigravity UA ride
///   on `runtime_headers`; the body wraps into the v1internal envelope and
///   the URL becomes `/v1internal:streamGenerateContent?alt=sse`.
/// * default API-key channel (or AI-Studio-style OAuth Bearer): plain Gemini
///   body on `/v1beta/models/{model}:streamGenerateContent?alt=sse` with
///   `?key=` (or bare Bearer headers) auth.
fn build_gemini_model_probe_request(
    base_url: &str,
    api_key: &str,
    runtime_headers: &HeaderMap,
    model: &str,
    channel: Option<&str>,
    antigravity_project: Option<&str>,
) -> anyhow::Result<(String, HeaderMap, Value)> {
    let base = base_url.trim_end_matches('/');
    let mut headers = HeaderMap::new();
    headers.extend(runtime_headers.clone());

    // sub2api's account-test payload: user turn + system instruction, no
    // generationConfig (the internal surface rejects surprises).
    let inner_body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "systemInstruction": {"parts": [{"text": "You are a helpful AI assistant."}]},
    });

    let is_antigravity = channel.is_some_and(|value| value.eq_ignore_ascii_case("antigravity"));
    if is_antigravity {
        let project_id = antigravity_project.ok_or_else(|| {
            anyhow::anyhow!(
                "google/antigravity credential is missing project_id; \
re-login the provider to re-run onboarding"
            )
        })?;
        let body =
            crate::provider::google::antigravity::wrap_request(inner_body, model, project_id);
        return Ok((
            format!("{base}/v1internal:streamGenerateContent?alt=sse"),
            headers,
            body,
        ));
    }

    if api_key.trim().is_empty() && !headers.contains_key(AUTHORIZATION) {
        anyhow::bail!("provider api key is empty");
    }
    let mut url = format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse");
    if !headers.contains_key(AUTHORIZATION) {
        url.push_str(&format!("?key={api_key}"));
    }
    Ok((url, headers, inner_body))
}

/// Probe one model with a minimal "hi" request (30s timeout).
#[allow(clippy::too_many_arguments)]
async fn probe_single_model(
    client: reqwest::Client,
    suite: crate::protocol::ids::Protocol,
    base_url: &str,
    api_key: &str,
    auth_scheme: &str,
    runtime_headers: &HeaderMap,
    model: &str,
    protocol_id: &str,
    fast_mode: bool,
    channel: Option<&str>,
    is_codex_oauth: bool,
    antigravity_project: Option<&str>,
) -> ProviderModelProbeResult {
    let start = Instant::now();

    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let (url, headers, body) = build_model_probe_request(
            suite,
            base_url,
            api_key,
            auth_scheme,
            runtime_headers,
            model,
            fast_mode,
            channel,
            is_codex_oauth,
            antigravity_project,
        )?;

        let response = client
            .post(url)
            .headers(headers)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            // Keep enough of the body for Google-style errors to surface their
            // details (links, verification hints) instead of cutting mid-word.
            let preview: String = body_text.chars().take(300).collect();
            let hint = if body_text.contains("Verify your account") {
                " — Google is gating this account: open https://antigravity.google \
                 (or gemini.google.com) in a browser with the SAME Google account \
                 and complete the verification prompt, then retry the test."
            } else {
                ""
            };
            anyhow::bail!("HTTP {status}: {preview}{hint}");
        }
        let body_text = response.text().await.unwrap_or_default();
        let reply = extract_probe_reply(&body_text)
            .ok_or_else(|| anyhow::anyhow!("response did not contain a readable reply"))?;
        Ok::<String, anyhow::Error>(reply)
    })
    .await;

    let (success, error, reply) = match outcome {
        Ok(Ok(reply)) => (true, None, Some(reply)),
        Ok(Err(error)) => (false, Some(error.to_string()), None),
        Err(_) => (false, Some("timeout after 30s".to_string()), None),
    };
    ProviderModelProbeResult {
        model: model.to_string(),
        success,
        error,
        latency_ms: start.elapsed().as_millis() as u64,
        protocol: protocol_id.to_string(),
        reply,
    }
}

/// Extract the assistant's text reply from a probe response body for any of
/// the three supported wire formats. Returns `None` when the response is
/// incomplete/failed or has no terminal event; a completed response with no
/// displayable text returns the callability marker "[completed]".
///
/// Reasoning models may spend the whole budget on thinking; when `content`
/// is empty the reasoning text is used as a fallback (prefixed with a marker
/// so the log makes clear it is a thought, not the final answer).
fn extract_probe_reply(body: &str) -> Option<String> {
    if let Some(reply) = extract_probe_sse_reply(body) {
        return Some(reply);
    }
    let json: Value = serde_json::from_str(body.trim_start()).ok()?;
    extract_probe_reply_json(&json)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeTerminal {
    Completed,
    Failed,
}

fn extract_probe_sse_reply(body: &str) -> Option<String> {
    // Handle CRLF, data-only events, multiline data, and an unterminated EOF
    // block. The first terminal event is authoritative.
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut text_delta = String::new();
    let mut text_fallback = String::new();
    let mut reasoning_delta = String::new();
    let mut reasoning_fallback = String::new();
    let mut terminal_response: Option<Value> = None;
    let mut terminal = None;

    for block in normalized.split("\n\n") {
        if terminal.is_some() {
            break;
        }
        let mut event_header = None;
        let mut data_lines = Vec::new();
        for raw_line in block.lines() {
            let line = raw_line.trim_start().trim_start_matches('\u{feff}');
            if let Some(event) = line.strip_prefix("event:") {
                event_header = Some(event.trim().to_string());
            } else if let Some(data) = line.strip_prefix("data:") {
                // SSE removes at most one optional space after the colon.
                data_lines.push(data.strip_prefix(' ').unwrap_or(data));
            }
        }
        if data_lines.is_empty() {
            continue;
        }
        let data = data_lines.join("\n");
        if data.trim() == "[DONE]" {
            continue;
        }

        let Ok(payload) = serde_json::from_str::<Value>(&data) else {
            let header_name = event_header.as_deref().filter(|name| !name.is_empty());
            if header_name.is_some_and(is_probe_terminal_event) {
                terminal = Some(ProbeTerminal::Failed);
            }
            continue;
        };

        // Gemini generateContent SSE chunks (plain or wrapped in the
        // v1internal `{"response": …}` envelope) carry no event name;
        // accumulate parts directly and treat finishReason as the terminal
        // marker. Detection requires `candidates` so codex-style
        // `{"type":…,"response":{…}}` events never match.
        let is_gemini_chunk = payload.get("candidates").is_some()
            || payload
                .get("response")
                .and_then(|inner| inner.get("candidates"))
                .is_some();
        if is_gemini_chunk {
            let gemini = payload
                .get("response")
                .filter(|inner| inner.is_object())
                .unwrap_or(&payload);
            let candidate = gemini
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|candidates| candidates.first());
            if let Some(parts) = candidate
                .and_then(|candidate| candidate.pointer("/content/parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
                    let Some(text) = part.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    if is_thought {
                        append_probe_candidate_text(&mut reasoning_delta, text.to_string());
                    } else {
                        append_probe_candidate_text(&mut text_delta, text.to_string());
                    }
                }
            }
            if candidate
                .and_then(|candidate| candidate.get("finishReason"))
                .is_some()
            {
                terminal = Some(ProbeTerminal::Completed);
            }
            continue;
        }

        let header_name = event_header.as_deref().filter(|name| !name.is_empty());
        let Some(event_name) = header_name
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .or_else(|| probe_status_event(&payload))
        else {
            continue;
        };

        match event_name {
            "response.output_text.delta" | "response.text.delta" => {
                append_probe_delta(&mut text_delta, payload.get("delta"));
            }
            "response.output_text.done" | "response.output_text" => {
                append_probe_candidate(
                    &mut text_fallback,
                    payload.get("text").or_else(|| payload.get("delta")),
                );
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning.delta" => {
                append_probe_delta(&mut reasoning_delta, payload.get("delta"));
            }
            "response.reasoning_summary_text.done"
            | "response.reasoning_text.done"
            | "response.reasoning.done" => {
                append_probe_candidate(
                    &mut reasoning_fallback,
                    payload.get("text").or_else(|| payload.get("delta")),
                );
            }
            "response.output_item.done" => {
                if let Some(item) = payload.get("item") {
                    append_probe_candidate_text(
                        &mut text_fallback,
                        extract_probe_content_text(item),
                    );
                    append_probe_candidate_text(
                        &mut reasoning_fallback,
                        extract_probe_reasoning_text(item),
                    );
                }
            }
            event if is_probe_terminal_event(event) => {
                let state = probe_terminal_state(event, &payload);
                terminal = Some(state);
                if state == ProbeTerminal::Completed {
                    terminal_response = Some(
                        payload
                            .get("response")
                            .cloned()
                            .unwrap_or_else(|| payload.clone()),
                    );
                }
            }
            _ => {}
        }
    }

    if terminal != Some(ProbeTerminal::Completed) {
        return None;
    }
    let terminal_text = terminal_response
        .as_ref()
        .map(extract_probe_responses_text)
        .unwrap_or_default();
    let terminal_reasoning = terminal_response
        .as_ref()
        .map(extract_probe_reasoning_text)
        .unwrap_or_default();

    // Ordered visible deltas win; complete values from done/terminal events
    // are fallbacks for providers that omit deltas.
    for text in [&text_delta, &text_fallback, &terminal_text] {
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }
    for reasoning in [&reasoning_delta, &reasoning_fallback, &terminal_reasoning] {
        if !reasoning.trim().is_empty() {
            return Some(format!("[thinking] {}", reasoning.trim()));
        }
    }

    // A completed response proves callability even when it has no display text.
    Some("[completed]".to_string())
}

fn probe_status_event(payload: &Value) -> Option<&'static str> {
    let status = payload
        .pointer("/response/status")
        .or_else(|| payload.get("status"))
        .and_then(Value::as_str);
    matches!(status, Some("completed" | "incomplete" | "failed")).then_some("response.done")
}

fn is_probe_terminal_event(event: &str) -> bool {
    matches!(
        event,
        "response.completed"
            | "response.done"
            | "response.incomplete"
            | "response.failed"
            | "response.cancelled"
            | "response.canceled"
            | "response.error"
            | "error"
    )
}

fn probe_terminal_state(event: &str, payload: &Value) -> ProbeTerminal {
    if matches!(
        event,
        "response.incomplete"
            | "response.failed"
            | "response.cancelled"
            | "response.canceled"
            | "response.error"
            | "error"
    ) {
        return ProbeTerminal::Failed;
    }
    match payload
        .pointer("/response/status")
        .or_else(|| payload.get("status"))
        .and_then(Value::as_str)
    {
        None | Some("completed") => ProbeTerminal::Completed,
        Some(_) => ProbeTerminal::Failed,
    }
}

fn append_probe_candidate_text(target: &mut String, text: String) {
    if text.trim().is_empty() {
        return;
    }
    // Done events may repeat a value already supplied by an item event.
    if target.trim().is_empty() {
        target.push_str(&text);
    } else if target != &text && !target.ends_with(&text) {
        if text.starts_with(target.as_str()) {
            target.clear();
            target.push_str(&text);
        } else {
            target.push_str(&text);
        }
    }
}

fn append_probe_candidate(target: &mut String, value: Option<&Value>) {
    if let Some(text) = value.and_then(probe_text_value) {
        append_probe_candidate_text(target, text);
    }
}

fn append_probe_delta(target: &mut String, value: Option<&Value>) {
    if let Some(text) = value.and_then(probe_delta_value) {
        // Delta fragments are ordered data, so preserve whitespace exactly.
        target.push_str(&text);
    }
}

fn probe_delta_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.to_string()),
        Value::Array(values) => {
            let mut text = String::new();
            let mut found = false;
            for value in values {
                if let Some(part) = probe_delta_value(value) {
                    found = true;
                    text.push_str(&part);
                }
            }
            found.then_some(text)
        }
        Value::Object(map) => map
            .get("value")
            .or_else(|| map.get("text"))
            .and_then(probe_delta_value),
        _ => None,
    }
}

/// Read a non-empty string or the occasional {value: ...} wrapper.
fn probe_text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.to_string()),
        Value::Array(values) => {
            let mut text = String::new();
            for value in values {
                if let Some(part) = probe_text_value(value) {
                    text.push_str(&part);
                }
            }
            (!text.trim().is_empty()).then_some(text)
        }
        Value::Object(map) => map
            .get("value")
            .or_else(|| map.get("text"))
            .and_then(probe_text_value),
        _ => None,
    }
}

/// Collect only known visible message/content part types.
fn append_probe_content_text(target: &mut String, value: &Value) {
    match value {
        Value::String(text) if !text.trim().is_empty() => target.push_str(text),
        Value::Array(values) => {
            for value in values {
                append_probe_content_text(target, value);
            }
        }
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = map.get("content") {
                    append_probe_content_text(target, content);
                }
            }
            Some("output_text" | "text" | "refusal") => {
                let value = map
                    .get("text")
                    .or_else(|| map.get("value"))
                    .or_else(|| map.get("refusal"));
                if let Some(text) = value.and_then(probe_text_value) {
                    target.push_str(&text);
                }
            }
            // Reasoning and tool-call items are not visible probe replies.
            Some(
                "reasoning" | "thinking" | "summary_text" | "reasoning_text" | "function_call"
                | "custom_tool_call",
            ) => {}
            _ => {}
        },
        _ => {}
    }
}

fn extract_probe_content_text(value: &Value) -> String {
    let mut text = String::new();
    append_probe_content_text(&mut text, value);
    text
}

fn extract_probe_responses_text(json: &Value) -> String {
    if let Some(text) = json
        .get("output_text")
        .and_then(probe_text_value)
        .filter(|text| !text.trim().is_empty())
    {
        return text;
    }
    json.get("output")
        .map(extract_probe_content_text)
        .unwrap_or_default()
}

/// Collect reasoning only from typed reasoning/thinking items or containers.
fn append_probe_reasoning_text(target: &mut String, value: &Value) {
    match value {
        Value::String(text) if !text.trim().is_empty() => {
            append_probe_candidate_text(target, text.to_string());
        }
        Value::Array(values) => {
            for value in values {
                append_probe_reasoning_text(target, value);
            }
        }
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                for field in ["summary", "content", "text", "value"] {
                    if let Some(value) = map.get(field) {
                        append_probe_reasoning_text(target, value);
                    }
                }
            }
            Some("thinking") => {
                for field in ["thinking", "text", "value"] {
                    if let Some(value) = map.get(field) {
                        append_probe_reasoning_text(target, value);
                    }
                }
            }
            Some("summary_text" | "reasoning_text") => {
                for field in ["text", "value"] {
                    if let Some(value) = map.get(field) {
                        append_probe_reasoning_text(target, value);
                    }
                }
            }
            // Tool calls and visible message parts never contribute reasoning.
            Some(
                "message" | "output_text" | "text" | "refusal" | "function_call"
                | "custom_tool_call",
            ) => {}
            _ => {
                for field in ["output", "reasoning", "reasoning_content", "thinking"] {
                    if let Some(value) = map.get(field) {
                        append_probe_reasoning_text(target, value);
                    }
                }
            }
        },
        _ => {}
    }
}

fn extract_probe_reasoning_text(value: &Value) -> String {
    let mut text = String::new();
    append_probe_reasoning_text(&mut text, value);
    text
}

fn extract_probe_reply_json(json: &Value) -> Option<String> {
    let raw = if json.get("choices").is_some() {
        // OpenAI chat.completions: visible content wins over thinking-only text.
        let message = json.pointer("/choices/0/message")?;
        let content = message
            .get("content")
            .map(extract_probe_content_text)
            .unwrap_or_default();
        if !content.trim().is_empty() {
            Some(content)
        } else {
            let reasoning = message
                .get("reasoning_content")
                .or_else(|| message.get("reasoning"))
                .map(extract_probe_reasoning_text)
                .unwrap_or_default();
            (!reasoning.trim().is_empty()).then(|| format!("[thinking] {reasoning}"))
        }
    } else if json.get("output").is_some() || json.get("output_text").is_some() {
        // Responses may expose output_text directly or nest typed content.
        if json
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed")
        {
            return None;
        }
        let text = extract_probe_responses_text(json);
        if !text.trim().is_empty() {
            Some(text)
        } else {
            let reasoning = extract_probe_reasoning_text(json);
            if !reasoning.trim().is_empty() {
                Some(format!("[thinking] {reasoning}"))
            } else if json.get("status").and_then(Value::as_str) == Some("completed") {
                Some("[completed]".to_string())
            } else {
                None
            }
        }
    } else if json.get("candidates").is_some() || json.get("response").is_some() {
        // Gemini generateContent — plain or wrapped in the v1internal
        // `{"response": …}` envelope (google/antigravity channel).
        let gemini = json
            .get("response")
            .filter(|inner| inner.is_object())
            .unwrap_or(json);
        let parts = gemini.pointer("/candidates/0/content/parts");
        let join_parts = |thought_only: bool| -> String {
            parts
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| {
                            // `thought` is absent on ordinary text parts.
                            let is_thought =
                                part.get("thought").and_then(Value::as_bool) == Some(true);
                            is_thought == thought_only
                        })
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default()
        };
        let text = join_parts(false);
        if !text.trim().is_empty() {
            Some(text)
        } else {
            let thinking = join_parts(true);
            if !thinking.trim().is_empty() {
                Some(format!("[thinking] {thinking}"))
            } else if gemini.pointer("/candidates/0/finishReason").is_some()
                || gemini.get("usageMetadata").is_some()
            {
                // HTTP 200 with a finished/usage-bearing envelope proves the
                // model is callable even when no text came back (the Code
                // Assist surface occasionally returns empty non-stream bodies).
                Some("[completed]".to_string())
            } else {
                None
            }
        }
    } else if json.get("content").is_some() {
        // Anthropic messages: visible text blocks win over thinking blocks.
        let text = json
            .get("content")
            .map(extract_probe_content_text)
            .unwrap_or_default();
        if !text.trim().is_empty() {
            Some(text)
        } else {
            let thinking = json
                .get("content")
                .map(extract_probe_reasoning_text)
                .unwrap_or_default();
            (!thinking.trim().is_empty()).then(|| format!("[thinking] {thinking}"))
        }
    } else if json.get("status").and_then(Value::as_str) == Some("completed") {
        Some("[completed]".to_string())
    } else {
        None
    }?;
    let trimmed = raw.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn normalize_protocol_config(
    mode: &str,
    default_protocol: &str,
    base_url: &str,
    api_key: &str,
    auth_mode: &str,
    endpoints: Vec<CreateProviderProtocolEndpoint>,
) -> anyhow::Result<NormalizedProtocolConfig> {
    let mode = match mode.trim() {
        "" | PROVIDER_PROTOCOL_MODE_FIXED => PROVIDER_PROTOCOL_MODE_FIXED,
        PROVIDER_PROTOCOL_MODE_ADAPTIVE => PROVIDER_PROTOCOL_MODE_ADAPTIVE,
        other => anyhow::bail!("unsupported provider protocol_mode: {other}"),
    };
    let registry = crate::protocol::registry::ProtocolRegistry::global();

    if mode == PROVIDER_PROTOCOL_MODE_FIXED {
        let protocol = registry
            .parse_protocol(default_protocol)
            .ok_or_else(|| anyhow::anyhow!("unsupported provider protocol: {default_protocol}"))?
            .as_str()
            .to_string();
        let base_url = normalize_endpoint_url(base_url)?;
        return Ok(NormalizedProtocolConfig {
            mode: mode.to_string(),
            default_protocol: protocol.clone(),
            base_url: base_url.clone(),
            api_key: api_key.to_string(),
            endpoints: vec![CreateProviderProtocolEndpoint {
                protocol,
                base_url,
                api_key: api_key.to_string(),
                auth_scheme: "auto".to_string(),
                is_enabled: true,
                priority: 0,
            }],
        });
    }

    if auth_mode.trim() != "apikey" {
        anyhow::bail!("adaptive protocol mode currently supports API key providers only");
    }
    if endpoints.is_empty() {
        anyhow::bail!("adaptive protocol mode requires at least one protocol endpoint");
    }

    let mut normalized = Vec::with_capacity(endpoints.len());
    let mut seen = std::collections::HashSet::new();
    for (index, endpoint) in endpoints.into_iter().enumerate() {
        let protocol = normalize_adaptive_endpoint_protocol(&endpoint.protocol)?;
        if !seen.insert(protocol.clone()) {
            anyhow::bail!("duplicate adaptive protocol endpoint: {protocol}");
        }
        let auth_scheme = match endpoint.auth_scheme.trim() {
            "" | "auto" => "auto",
            "bearer" => "bearer",
            "x-api-key" => "x-api-key",
            "query" => "query",
            "none" => "none",
            other => anyhow::bail!("unsupported endpoint auth_scheme: {other}"),
        };
        if endpoint.api_key.trim().is_empty() && auth_scheme != "none" {
            anyhow::bail!("API key is required for adaptive endpoint {protocol}");
        }
        normalized.push(CreateProviderProtocolEndpoint {
            protocol,
            base_url: normalize_endpoint_url(&endpoint.base_url)?,
            api_key: endpoint.api_key,
            auth_scheme: auth_scheme.to_string(),
            is_enabled: endpoint.is_enabled,
            priority: if endpoint.priority == 0 {
                index as i32
            } else {
                endpoint.priority
            },
        });
    }

    if !normalized.iter().any(|endpoint| endpoint.is_enabled) {
        anyhow::bail!("adaptive protocol mode requires at least one enabled endpoint");
    }

    let default_endpoint = resolve_default_adaptive_endpoint(default_protocol, &normalized)?;
    Ok(NormalizedProtocolConfig {
        mode: mode.to_string(),
        default_protocol: default_endpoint.protocol.clone(),
        base_url: default_endpoint.base_url.clone(),
        api_key: default_endpoint.api_key.clone(),
        endpoints: normalized,
    })
}

fn normalize_adaptive_endpoint_protocol(raw: &str) -> anyhow::Result<String> {
    let registry = crate::protocol::registry::ProtocolRegistry::global();
    if let Some(endpoint) = registry.resolve_alias(raw) {
        return Ok(endpoint.to_string());
    }
    let protocol = registry
        .parse_protocol(raw)
        .ok_or_else(|| anyhow::anyhow!("unsupported protocol endpoint: {raw}"))?;
    let endpoints = registry.list_by_protocol(protocol);
    if endpoints.len() != 1 {
        anyhow::bail!(
            "protocol '{raw}' has multiple endpoints; select a concrete protocol endpoint"
        );
    }
    Ok(endpoints[0].id().to_string())
}

fn resolve_default_adaptive_endpoint<'a>(
    raw: &str,
    endpoints: &'a [CreateProviderProtocolEndpoint],
) -> anyhow::Result<&'a CreateProviderProtocolEndpoint> {
    let registry = crate::protocol::registry::ProtocolRegistry::global();
    if let Some(default) = registry.resolve_alias(raw) {
        let canonical = default.to_string();
        return endpoints
            .iter()
            .find(|endpoint| endpoint.is_enabled && endpoint.protocol == canonical)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "default protocol endpoint is not configured or enabled: {canonical}"
                )
            });
    }

    let suite = registry
        .parse_protocol(raw)
        .ok_or_else(|| anyhow::anyhow!("unsupported default protocol: {raw}"))?;
    let matches = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.is_enabled
                && registry
                    .resolve_alias(&endpoint.protocol)
                    .is_some_and(|candidate| candidate.protocol == suite)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [endpoint] => Ok(*endpoint),
        [] => anyhow::bail!("default protocol has no configured endpoint: {raw}"),
        _ => anyhow::bail!(
            "default protocol '{raw}' is ambiguous; select a concrete protocol endpoint"
        ),
    }
}

fn normalize_endpoint_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed)
        .map_err(|_| anyhow::anyhow!("invalid provider Base URL: {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("provider Base URL must use http or https: {raw}");
    }
    Ok(trimmed.to_string())
}

fn ensure_adaptive_provider_supported(
    mode: &str,
    vendor: Option<&str>,
    preset_key: Option<&str>,
) -> anyhow::Result<()> {
    if mode.trim() != PROVIDER_PROTOCOL_MODE_ADAPTIVE {
        return Ok(());
    }
    let is_vertex = [vendor, preset_key]
        .into_iter()
        .flatten()
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case("vertexai"));
    if is_vertex {
        anyhow::bail!("adaptive protocol mode does not support Vertex AI providers");
    }
    Ok(())
}

impl AdminService {
    // ── Providers ──

    pub async fn list_providers(&self) -> anyhow::Result<Vec<Provider>> {
        self.gw.storage.providers().list().await
    }

    pub async fn list_provider_presets(&self) -> anyhow::Result<Vec<Value>> {
        parse_provider_presets_snapshot()
    }

    pub async fn get_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.gw
            .storage
            .providers()
            .get(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("provider not found: {id}"))
    }
    pub async fn create_provider(&self, input: CreateProvider) -> anyhow::Result<Provider> {
        let name = normalize_name(&input.name, "provider name")?;
        self.ensure_provider_name_unique(None, &name).await?;
        let vendor = normalize_vendor(input.vendor.as_deref());
        ensure_adaptive_provider_supported(
            &input.protocol_mode,
            vendor.as_deref(),
            input.preset_key.as_deref(),
        )?;
        let auth_mode = resolve_admin_preset_channel_auth_mode(
            input.preset_key.as_deref(),
            input.channel.as_deref(),
        )
        .unwrap_or(input.auth_mode);
        let api_key = if auth_mode == "oauth" {
            String::new()
        } else {
            input.api_key
        };
        let protocol = normalize_protocol_config(
            &input.protocol_mode,
            &input.protocol,
            &input.base_url,
            &api_key,
            &auth_mode,
            input.protocol_endpoints,
        )?;
        let provider = self
            .gw
            .storage
            .providers()
            .create(CreateProvider {
                name,
                vendor,
                protocol: protocol.default_protocol,
                base_url: protocol.base_url,
                protocol_mode: protocol.mode,
                protocol_endpoints: protocol.endpoints,
                preset_key: input.preset_key,
                channel: input.channel,
                models_source: input.models_source,
                static_models: input.static_models,
                api_key: protocol.api_key,
                auth_mode,
                use_proxy: input.use_proxy,
                fast_mode: input.fast_mode,
            })
            .await?;

        let provider = if provider.is_adaptive() {
            let _ = self.test_provider(&provider.id).await?;
            self.get_provider(&provider.id).await?
        } else {
            provider
        };
        self.gw.quota_registry.request_refresh(&provider.id);
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn copy_provider(&self, id: &str) -> anyhow::Result<Provider> {
        self.copy_provider_with_options(id, CopyProviderOptions::default())
            .await
    }

    pub async fn copy_provider_with_options(
        &self,
        id: &str,
        options: CopyProviderOptions,
    ) -> anyhow::Result<Provider> {
        let original = self.get_provider(id).await?;
        // Snapshot all saved ratings, independently of catalog availability or
        // append_targets. A read failure must not produce an unscored copy.
        let rating_snapshot = match self.gw.storage.provider_model_ratings() {
            Some(store) => Some(store.list(Some(&original.id)).await?),
            None => None,
        };
        let name = self.next_provider_copy_name(&original.name).await?;
        let copied = self
            .create_provider(CreateProvider {
                name,
                vendor: original.vendor.clone(),
                protocol: original.protocol.clone(),
                base_url: original.base_url.clone(),
                protocol_mode: original.protocol_mode.clone(),
                protocol_endpoints: original
                    .protocol_endpoints
                    .iter()
                    .map(|endpoint| CreateProviderProtocolEndpoint {
                        protocol: endpoint.protocol.clone(),
                        base_url: endpoint.base_url.clone(),
                        api_key: endpoint.api_key.clone(),
                        auth_scheme: endpoint.auth_scheme.clone(),
                        is_enabled: endpoint.is_enabled,
                        priority: endpoint.priority,
                    })
                    .collect(),
                preset_key: original.preset_key.clone(),
                channel: original.channel.clone(),
                models_source: original.models_source.clone(),
                static_models: original.static_models.clone(),
                api_key: original.api_key.clone(),
                auth_mode: original.auth_mode.clone(),
                use_proxy: original.use_proxy,
                fast_mode: original.fast_mode,
            })
            .await?;
        let copied = self
            .update_provider(
                &copied.id,
                UpdateProvider {
                    is_enabled: Some(false),
                    ..Default::default()
                },
            )
            .await?;

        let copied = if original.effective_auth_mode() == "oauth" {
            match self
                .gw
                .storage
                .oauth_credentials()
                .get(&original.id)
                .await?
            {
                Some(credential) => {
                    let credential_input = upsert_credential_from_oauth(&credential);
                    let provisioned = async {
                        self.gw
                            .storage
                            .oauth_credentials()
                            .upsert(&copied.id, credential_input)
                            .await?;
                        let driver_key = credential.driver_key.clone();
                        let stored = stored_credential_from_oauth(&credential, &driver_key);
                        self.sync_provider_runtime_fields(&copied, &stored).await
                    }
                    .await;

                    match provisioned {
                        Ok(provider) => provider,
                        Err(error) => {
                            if let Err(cleanup_error) = self.delete_provider(&copied.id).await {
                                tracing::warn!(
                                    "failed to rollback copied oauth provider {} after provisioning error: {}",
                                    copied.id,
                                    cleanup_error
                                );
                            }
                            return Err(error.context("copy oauth provider"));
                        }
                    }
                }
                None => copied,
            }
        } else {
            copied
        };

        if let Some(snapshot) = rating_snapshot {
            if let Err(error) = self.rating_store()?.restore(&copied.id, &snapshot).await {
                if let Err(cleanup_error) = self.delete_provider(&copied.id).await {
                    return Err(error.context(format!(
                        "Rating copy failed; rollback of provider {} also failed: {cleanup_error}",
                        copied.id
                    )));
                }
                return Err(error.context("Rating copy failed; the new provider was rolled back"));
            }
        }

        if options.append_targets {
            self.append_provider_targets(&original.id, &copied.id)
                .await?;
        }

        Ok(copied)
    }

    pub async fn update_provider(
        &self,
        id: &str,
        input: UpdateProvider,
    ) -> anyhow::Result<Provider> {
        let current = self.get_provider(id).await?;
        let current_base_url = current.base_url.clone();
        let protocol_config_changed = input.protocol_mode.is_some()
            || input.protocol.is_some()
            || input.base_url.is_some()
            || input.api_key.is_some()
            || input.auth_mode.is_some()
            || input.protocol_endpoints.is_some();
        let quota_config_changed = protocol_config_changed
            || input.vendor.is_some()
            || input.preset_key.is_some()
            || input.channel.is_some()
            || input.is_enabled.is_some();
        let models_source_input = input
            .models_source
            .clone()
            .map(|value| value.trim().to_string());

        let name = normalize_name(
            &input.name.clone().unwrap_or_else(|| current.name.clone()),
            "provider name",
        )?;
        self.ensure_provider_name_unique(Some(id), &name).await?;
        let vendor = if input.vendor.is_some() {
            normalize_vendor(input.vendor.as_deref())
        } else {
            normalize_vendor(current.vendor.as_deref())
        };
        let models_source = models_source_input
            .or_else(|| current.models_source.as_deref().map(ToString::to_string));
        let raw_protocol = input
            .protocol
            .clone()
            .unwrap_or_else(|| current.protocol.clone());
        let raw_base_url = input
            .base_url
            .clone()
            .unwrap_or_else(|| current.base_url.clone());
        let preset_key = input.preset_key.clone().or(current.preset_key.clone());
        let channel = input.channel.clone().or(current.channel.clone());
        let static_models = input
            .static_models
            .clone()
            .or(current.static_models.clone());
        let raw_api_key = input
            .api_key
            .clone()
            .unwrap_or_else(|| current.api_key.clone());
        let auth_mode =
            resolve_admin_preset_channel_auth_mode(preset_key.as_deref(), channel.as_deref())
                .or(input.auth_mode.clone())
                .unwrap_or_else(|| current.auth_mode.clone());
        let raw_api_key = if auth_mode == "oauth" {
            String::new()
        } else {
            raw_api_key
        };
        let protocol_mode = input
            .protocol_mode
            .as_deref()
            .unwrap_or(&current.protocol_mode);
        ensure_adaptive_provider_supported(
            protocol_mode,
            vendor.as_deref(),
            preset_key.as_deref(),
        )?;
        let raw_endpoints = input.protocol_endpoints.clone().unwrap_or_else(|| {
            current
                .protocol_endpoints
                .iter()
                .map(|endpoint| CreateProviderProtocolEndpoint {
                    protocol: endpoint.protocol.clone(),
                    base_url: endpoint.base_url.clone(),
                    api_key: endpoint.api_key.clone(),
                    auth_scheme: endpoint.auth_scheme.clone(),
                    is_enabled: endpoint.is_enabled,
                    priority: endpoint.priority,
                })
                .collect()
        });
        let protocol = normalize_protocol_config(
            protocol_mode,
            &raw_protocol,
            &raw_base_url,
            &raw_api_key,
            &auth_mode,
            raw_endpoints,
        )?;
        let use_proxy = input.use_proxy.unwrap_or(current.use_proxy);
        let fast_mode = input.fast_mode.unwrap_or(current.fast_mode);
        let is_enabled = input.is_enabled.unwrap_or(current.is_enabled);
        let base_url_changed = protocol.base_url != current_base_url;

        let mut provider = self
            .gw
            .storage
            .providers()
            .update(
                id,
                UpdateProvider {
                    name: Some(name),
                    vendor,
                    protocol: Some(protocol.default_protocol),
                    base_url: Some(protocol.base_url),
                    protocol_mode: Some(protocol.mode),
                    protocol_endpoints: protocol_config_changed.then_some(protocol.endpoints),
                    preset_key,
                    channel,
                    models_source,
                    static_models,
                    api_key: Some(protocol.api_key),
                    auth_mode: Some(auth_mode),
                    use_proxy: Some(use_proxy),
                    fast_mode: Some(fast_mode),
                    is_enabled: Some(is_enabled),
                },
            )
            .await?;

        if base_url_changed {
            self.gw.clear_ollama_capability_cache_for_provider(id).await;
        }

        if protocol_config_changed && provider.is_adaptive() {
            let _ = self.test_provider(id).await?;
            provider = self.get_provider(id).await?;
        }

        if quota_config_changed {
            if provider.is_enabled {
                self.gw.quota_registry.invalidate(id);
            } else {
                self.gw.quota_registry.remove(id);
            }
        }
        self.bump_config_epoch().await?;
        Ok(provider)
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.gw.storage.providers().delete(id).await?;
        self.reload_model_cache().await?;
        self.bump_config_epoch().await?;
        self.gw.clear_ollama_capability_cache_for_provider(id).await;
        self.gw.quota_registry.remove(id);
        Ok(())
    }

    async fn ensure_provider_name_unique(
        &self,
        exclude_id: Option<&str>,
        name: &str,
    ) -> anyhow::Result<()> {
        if self
            .gw
            .storage
            .providers()
            .exists_by_name(name, exclude_id)
            .await?
        {
            return Err(coded_error(
                "PROVIDER_NAME_CONFLICT",
                &format!("provider name already exists: {name}"),
                serde_json::json!({ "name": name }),
            ));
        }
        Ok(())
    }

    async fn next_provider_copy_name(&self, original_name: &str) -> anyhow::Result<String> {
        let base = format!("{}_Copy", normalize_name(original_name, "provider name")?);
        if !self
            .gw
            .storage
            .providers()
            .exists_by_name(&base, None)
            .await?
        {
            return Ok(base);
        }

        for index in 2.. {
            let candidate = format!("{base}{index}");
            if !self
                .gw
                .storage
                .providers()
                .exists_by_name(&candidate, None)
                .await?
            {
                return Ok(candidate);
            }
        }

        unreachable!("unbounded provider copy name search must return");
    }

    async fn append_provider_targets(
        &self,
        original_provider_id: &str,
        copied_provider_id: &str,
    ) -> anyhow::Result<()> {
        let models = self.list_models().await?;
        for model in models.into_iter().filter(|model| {
            model
                .targets
                .iter()
                .any(|target| target.provider_id == original_provider_id)
        }) {
            let mut targets = model
                .targets
                .iter()
                .map(|target| CreateModelBackend {
                    provider_id: target.provider_id.clone(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                    is_fallback: Some(target.is_fallback),
                })
                .collect::<Vec<_>>();

            let copied_targets = model
                .targets
                .iter()
                .filter(|target| target.provider_id == original_provider_id)
                .map(|target| CreateModelBackend {
                    provider_id: copied_provider_id.to_string(),
                    model: target.model.clone(),
                    weight: Some(target.weight),
                    priority: Some(target.priority),
                    // Duplicated rows become regular targets: a second
                    // fallback row would violate the one-fallback rule.
                    is_fallback: Some(false),
                });
            targets.extend(copied_targets);

            self.update_model(
                &model.id,
                UpdateModel {
                    targets: Some(
                        targets
                            .into_iter()
                            .map(|target| UpsertModelBackend {
                                id: None,
                                provider_id: target.provider_id,
                                model: target.model,
                                weight: target.weight,
                                priority: target.priority,
                                is_fallback: target.is_fallback,
                            })
                            .collect(),
                    ),
                    ..UpdateModel::default()
                },
            )
            .await?;
        }
        Ok(())
    }
    pub async fn test_provider(&self, id: &str) -> anyhow::Result<TestResult> {
        let provider = self.get_provider(id).await?;
        self.gw
            .clear_ollama_capability_cache_for_provider(&provider.id)
            .await;
        if provider.is_adaptive() {
            let result = self.test_adaptive_provider_endpoints(&provider).await?;
            self.record_provider_test_result(&provider.id, &result)
                .await?;
            return Ok(result);
        }
        let start = Instant::now();
        let protocol = provider.protocol.trim();
        let vertex_runtime = if vertexai::is_vertex_vendor(&provider) {
            Some(self.resolve_provider_runtime(&provider).await?)
        } else {
            None
        };
        let base_url_owned = vertex_runtime
            .as_ref()
            .and_then(|runtime| runtime.binding.base_url_override.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| provider.base_url.clone());
        let base_url = base_url_owned.trim();

        let result = if base_url.is_empty() {
            TestResult {
                success: false,
                latency_ms: 0,
                model: None,
                error: Some("Base URL is empty".to_string()),
                endpoints: Vec::new(),
            }
        } else {
            let mut failures: Vec<String> = Vec::new();
            if reqwest::Url::parse(base_url).is_err() {
                failures.push(format!("{protocol}: Base URL format is invalid"));
            } else {
                let mut request = self
                    .gw
                    .http_client
                    .get(base_url)
                    .timeout(Duration::from_secs(10));
                if let Some(runtime) = &vertex_runtime {
                    let mut headers = runtime_binding_headers(&runtime.binding)?;
                    if !runtime.binding.disable_default_auth {
                        headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {}", runtime.access_token))?,
                        );
                    }
                    request = request.headers(headers);
                }
                if let Err(e) = request.send().await {
                    failures.push(format!("{protocol}: {}", format_connectivity_error(&e)));
                }
            }

            if failures.is_empty() {
                TestResult {
                    success: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: None,
                    endpoints: Vec::new(),
                }
            } else {
                TestResult {
                    success: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    model: None,
                    error: Some(format!(
                        "Connectivity check failed for provider endpoint: {}",
                        failures.join("; ")
                    )),
                    endpoints: Vec::new(),
                }
            }
        };
        self.record_provider_test_result(&provider.id, &result)
            .await?;
        Ok(result)
    }

    async fn test_adaptive_provider_endpoints(
        &self,
        provider: &Provider,
    ) -> anyhow::Result<TestResult> {
        let start = Instant::now();
        let endpoints = provider
            .protocol_endpoints
            .iter()
            .filter(|endpoint| endpoint.is_enabled)
            .cloned()
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Ok(TestResult {
                success: false,
                latency_ms: 0,
                model: None,
                error: Some("Adaptive provider has no enabled protocol endpoints".to_string()),
                endpoints: Vec::new(),
            });
        }

        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let results = futures::future::join_all(
            endpoints
                .into_iter()
                .map(|endpoint| self.test_adaptive_endpoint(client.clone(), endpoint)),
        )
        .await;

        let mut endpoint_results = Vec::with_capacity(results.len());
        for result in results {
            self.gw
                .storage
                .providers()
                .record_endpoint_test_result(
                    &result.endpoint_id,
                    ProviderEndpointTestResult {
                        success: result.success,
                        error: result.error.clone(),
                        tested_at: result.tested_at.clone(),
                    },
                )
                .await?;
            endpoint_results.push(result);
        }

        let failures = endpoint_results
            .iter()
            .filter(|result| !result.success)
            .map(|result| {
                format!(
                    "{}: {}",
                    result.protocol,
                    result.error.as_deref().unwrap_or("connection failed")
                )
            })
            .collect::<Vec<_>>();
        Ok(TestResult {
            success: failures.is_empty(),
            latency_ms: start.elapsed().as_millis() as u64,
            model: None,
            error: (!failures.is_empty()).then(|| {
                format!(
                    "Connectivity check failed for adaptive endpoint(s): {}",
                    failures.join("; ")
                )
            }),
            endpoints: endpoint_results,
        })
    }

    async fn test_adaptive_endpoint(
        &self,
        client: reqwest::Client,
        endpoint: ProviderProtocolEndpoint,
    ) -> EndpointTestResult {
        let start = Instant::now();
        let tested_at = Utc::now().to_rfc3339();
        let outcome = async {
            let mut url = reqwest::Url::parse(endpoint.base_url.trim())?;
            let protocol = crate::protocol::registry::ProtocolRegistry::global()
                .resolve_alias(&endpoint.protocol)
                .ok_or_else(|| anyhow::anyhow!("unsupported protocol endpoint"))?;
            let auth_scheme = match endpoint.auth_scheme.trim() {
                "" | "auto" => match protocol.protocol {
                    crate::protocol::ids::Protocol::AnthropicMessages => "x-api-key",
                    crate::protocol::ids::Protocol::GoogleGemini => "query",
                    _ => "bearer",
                },
                explicit => explicit,
            };
            let mut headers = HeaderMap::new();
            match auth_scheme {
                "bearer" => {
                    headers.insert(
                        AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {}", endpoint.api_key))?,
                    );
                }
                "x-api-key" => {
                    headers.insert("x-api-key", HeaderValue::from_str(&endpoint.api_key)?);
                    if protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages {
                        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                    }
                }
                "query" => {
                    url.query_pairs_mut().append_pair("key", &endpoint.api_key);
                }
                "none" => {}
                other => anyhow::bail!("unsupported endpoint auth scheme: {other}"),
            }

            let response = client
                .get(url)
                .headers(headers)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|error| anyhow::anyhow!(format_connectivity_error(&error)))?;
            let status = response.status();
            if matches!(status.as_u16(), 401 | 403) || status.is_server_error() {
                anyhow::bail!("HTTP {status}");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        EndpointTestResult {
            endpoint_id: endpoint.id,
            protocol: endpoint.protocol,
            base_url: endpoint.base_url,
            success: outcome.is_ok(),
            latency_ms: start.elapsed().as_millis() as u64,
            error: outcome.err().map(|error| error.to_string()),
            tested_at,
        }
    }

    /// google/antigravity: per-account dynamic model discovery via
    /// `v1internal:fetchAvailableModels` — the authoritative subscription
    /// catalog (newer models appear here before any static list ships).
    /// Best-effort callers retain the curated fallback; strict directory callers
    /// must not confuse failed discovery with a successful static catalog.
    async fn antigravity_available_models(
        &self,
        provider: &Provider,
        runtime: Option<&ResolvedProviderRuntime>,
        require_catalog: bool,
    ) -> anyhow::Result<Option<Vec<String>>> {
        if !crate::provider::google::antigravity::is_google_antigravity(provider) {
            return Ok(None);
        }
        let discovery = async {
            let runtime = runtime.context("Antigravity runtime unavailable")?;
            let credential = runtime
                .credential
                .as_ref()
                .context("Antigravity credential unavailable")?;
            let project_id =
                crate::provider::google::antigravity::antigravity_project_id(Some(credential))?;
            let token = runtime.access_token.trim();
            anyhow::ensure!(!token.is_empty(), "Antigravity access token unavailable");
            let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
            crate::provider::google::antigravity::fetch_available_models(
                &client,
                token,
                &project_id,
            )
            .await
        }
        .await;
        match discovery {
            Ok(models) if require_catalog || !models.is_empty() => Ok(Some(models)),
            Ok(_) => Ok(None),
            Err(_) if require_catalog => {
                anyhow::bail!("Antigravity model catalog could not be loaded")
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    provider = %provider.id,
                    "antigravity fetchAvailableModels failed; falling back to the static model list"
                );
                Ok(None)
            }
        }
    }

    /// Send a minimal "hi" chat request to every model in the provider's
    /// discovered list and report which ones actually answer. The WebUI uses
    /// the results to hide non-callable models from route target pickers.
    pub async fn probe_provider_models(
        &self,
        id: &str,
    ) -> anyhow::Result<ProviderModelProbeOutcome> {
        use futures::StreamExt;

        let provider = self.get_provider(id).await?;
        let models = self.get_provider_models(id).await?;
        if models.is_empty() {
            anyhow::bail!("provider model list is empty");
        }

        // Pick the probe endpoint: adaptive providers probe through the
        // endpoint matching their configured default protocol
        // (`provider.protocol`); if the default is disabled or missing, fall
        // back to the first enabled endpoint. Fixed providers probe through
        // their single configuration. Fixed OAuth providers resolve the same
        // refreshed token, base URL, and identity headers used by dispatch.
        let registry = crate::protocol::registry::ProtocolRegistry::global();
        let runtime = if provider.is_adaptive() {
            None
        } else {
            Some(self.resolve_provider_runtime(&provider).await?)
        };
        // google/antigravity probes need the companion project id from the
        // OAuth credential metadata; other channels leave this None. Computed
        // before the runtime is destructured below.
        let antigravity_project: Option<String> = runtime
            .as_ref()
            .and_then(|runtime| runtime.credential.as_ref())
            .and_then(|credential| {
                crate::provider::google::antigravity::antigravity_project_id(Some(credential)).ok()
            });
        let (suite_raw, base_url, api_key, auth_scheme, runtime_headers) = if provider.is_adaptive()
        {
            let enabled: Vec<&ProviderProtocolEndpoint> = provider
                .protocol_endpoints
                .iter()
                .filter(|endpoint| endpoint.is_enabled)
                .collect();
            if enabled.is_empty() {
                anyhow::bail!("provider has no enabled protocol endpoints");
            }
            let preferred = enabled
                .iter()
                .find(|endpoint| endpoint.protocol == provider.protocol)
                .or_else(|| enabled.first())
                .expect("enabled endpoints is non-empty");
            (
                preferred.protocol.clone(),
                preferred.base_url.clone(),
                preferred.api_key.clone(),
                preferred.auth_scheme.clone(),
                HeaderMap::new(),
            )
        } else {
            let runtime = runtime.expect("fixed provider runtime was resolved above");
            let base_url = runtime
                .binding
                .base_url_override
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| provider.base_url.clone());
            let auth_scheme = if runtime.binding.disable_default_auth {
                "none".to_string()
            } else {
                "auto".to_string()
            };
            (
                provider.protocol.clone(),
                base_url,
                runtime.access_token,
                auth_scheme,
                runtime_binding_headers(&runtime.binding)?,
            )
        };

        let suite = registry
            .parse_protocol(&suite_raw)
            .ok_or_else(|| anyhow::anyhow!("unsupported provider protocol: {suite_raw}"))?;
        if base_url.trim().is_empty() {
            anyhow::bail!("provider base URL is empty");
        }
        if api_key.trim().is_empty()
            && auth_scheme.trim() != "none"
            && !runtime_headers.contains_key(AUTHORIZATION)
        {
            anyhow::bail!("provider api key is empty");
        }

        let effective_scheme = match auth_scheme.trim() {
            "" | "auto" => match suite {
                crate::protocol::ids::Protocol::AnthropicMessages => "x-api-key",
                _ => "bearer",
            },
            explicit => explicit,
        };

        let is_codex_oauth = provider.effective_auth_mode().trim() == "oauth"
            && provider
                .vendor
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("openai"))
            && provider
                .channel
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("codex"));
        let client = self.gw.http_client_for_provider(provider.use_proxy).await?;
        let base_url = base_url.trim().to_string();
        let api_key = api_key.trim().to_string();
        let protocol_id = registry
            .resolve_alias(&suite_raw)
            .map(|endpoint| endpoint.to_string())
            .unwrap_or_else(|| suite_raw.clone());
        let fast_mode = provider.fast_mode;
        let channel = provider.channel.clone();

        let mut results: Vec<ProviderModelProbeResult> = futures::stream::iter(models)
            .map(|model| {
                let client = client.clone();
                let base_url = base_url.clone();
                let api_key = api_key.clone();
                let scheme = effective_scheme.to_string();
                let runtime_headers = runtime_headers.clone();
                let protocol_id = protocol_id.clone();
                let channel = channel.clone();
                let antigravity_project = antigravity_project.clone();
                async move {
                    probe_single_model(
                        client,
                        suite,
                        &base_url,
                        &api_key,
                        &scheme,
                        &runtime_headers,
                        &model,
                        &protocol_id,
                        fast_mode,
                        channel.as_deref(),
                        is_codex_oauth,
                        antigravity_project.as_deref(),
                    )
                    .await
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;
        results.sort_by(|a, b| a.model.cmp(&b.model));
        Ok(ProviderModelProbeOutcome {
            meta: ProviderModelProbeMeta {
                protocol: protocol_id,
                base_url,
            },
            results,
        })
    }

    async fn record_provider_test_result(
        &self,
        provider_id: &str,
        result: &TestResult,
    ) -> anyhow::Result<()> {
        self.gw
            .storage
            .providers()
            .record_test_result(
                provider_id,
                ProviderTestResult {
                    success: result.success,
                    tested_at: String::new(),
                },
            )
            .await
    }

    pub async fn test_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        // Adaptive providers: the discovery endpoint is OpenAI-style even when
        // the default protocol is not — authenticate with an enabled
        // OpenAI-family endpoint's Bearer key instead of the default
        // protocol's scheme (e.g. Anthropic `x-api-key`).
        let (auth_protocol, auth_credential) = match adaptive_model_fetch_auth(&provider) {
            Some((protocol, api_key)) => (protocol, api_key),
            None => (provider.protocol.clone(), credential),
        };
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models: Vec<String> = static_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }
        let endpoint = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| provider.effective_models_source().map(ToString::to_string))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Model Discovery URL is empty"))?;

        if let Some(models) = lookup_models_dev_models(&self.gw.config.data_dir, &endpoint)? {
            if models.is_empty() {
                anyhow::bail!("Model list format is invalid or empty");
            }
            return Ok(models);
        }

        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(&endpoint)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if is_google_protocol(&auth_protocol) && !runtime.binding.disable_default_auth {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{endpoint}{separator}key={}", auth_credential))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let preview = body.chars().take(200).collect::<String>();
            anyhow::bail!("HTTP {status}: {preview}");
        }

        let json: Value = resp.json().await.unwrap_or_default();
        let models =
            extract_models_from_response(&provider.protocol, provider.vendor.as_deref(), &json);
        if models.is_empty() {
            anyhow::bail!("Model list format is invalid or empty");
        }

        Ok(merge_model_lists(models, preset_extra_models(&provider)))
    }
    pub async fn get_provider_models(&self, id: &str) -> anyhow::Result<Vec<String>> {
        self.get_provider_models_with_catalog_validation(id, false)
            .await
    }

    /// Rating management needs to distinguish an empty catalog from an outage.
    /// Existing callers retain their best-effort/static fallback behavior.
    pub async fn get_provider_models_with_catalog_validation(
        &self,
        id: &str,
        require_catalog: bool,
    ) -> anyhow::Result<Vec<String>> {
        let provider = self.get_provider(id).await?;
        let runtime = self.resolve_provider_runtime(&provider).await?;
        let credential = runtime.access_token.clone();
        // Same adaptive-auth rationale as `test_provider_models` above.
        let (auth_protocol, auth_credential) = match adaptive_model_fetch_auth(&provider) {
            Some((protocol, api_key)) => (protocol, api_key),
            None => (provider.protocol.clone(), credential),
        };
        // Dynamic per-account catalog first; static curated list is fallback.
        if let Some(models) = self
            .antigravity_available_models(&provider, Some(&runtime), require_catalog)
            .await?
        {
            return Ok(merge_model_lists(models, preset_extra_models(&provider)));
        }
        if let Some(static_list) = runtime.binding.static_models_override.as_deref() {
            let models: Vec<String> = static_list
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !models.is_empty() {
                return Ok(models);
            }
        }

        if let Some(endpoint) = runtime
            .binding
            .models_source_override
            .clone()
            .or_else(|| resolve_models_endpoint(&provider))
        {
            if let Some(models) = lookup_models_dev_models(&self.gw.config.data_dir, &endpoint)?
                && !models.is_empty()
            {
                return Ok(models);
            }

            let mut headers = if runtime.binding.disable_default_auth {
                HeaderMap::new()
            } else {
                build_model_headers(&auth_protocol, provider.vendor.as_deref(), &auth_credential)?
            };
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            let mut request = self.gw.http_client.get(&endpoint).headers(headers);

            if is_google_protocol(&auth_protocol) && !runtime.binding.disable_default_auth {
                let separator = if endpoint.contains('?') { '&' } else { '?' };
                let mut headers = build_model_headers(
                    &auth_protocol,
                    provider.vendor.as_deref(),
                    &auth_credential,
                )?;
                headers.extend(runtime_binding_headers(&runtime.binding)?);
                request = self
                    .gw
                    .http_client
                    .get(format!("{endpoint}{separator}key={}", auth_credential))
                    .headers(headers);
            }

            if require_catalog {
                let response = request
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Model catalog could not be loaded"))?;
                if !response.status().is_success() {
                    anyhow::bail!("Model catalog returned HTTP {}", response.status().as_u16());
                }
                let json: Value = response
                    .json()
                    .await
                    .map_err(|_| anyhow::anyhow!("Model catalog returned invalid JSON"))?;
                let data = json.get("data").and_then(Value::as_array);
                let models = json.get("models").and_then(Value::as_array);
                anyhow::ensure!(
                    data.is_some() || models.is_some(),
                    "Model catalog response has no model list"
                );
                if let Some(entries) = data {
                    anyhow::ensure!(
                        entries.iter().all(|entry| entry
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| !id.trim().is_empty())),
                        "Model catalog contains an invalid model identifier"
                    );
                }
                if let Some(entries) = models {
                    anyhow::ensure!(
                        entries
                            .iter()
                            .all(|entry| ["name", "slug", "id"].iter().any(|key| entry
                                .get(key)
                                .and_then(Value::as_str)
                                .is_some_and(|id| !id.trim().is_empty()))),
                        "Model catalog contains an invalid model identifier"
                    );
                }
                let models = extract_models_from_response(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &json,
                );
                return Ok(merge_model_lists(models, preset_extra_models(&provider)));
            }

            if let Ok(resp) = request.send().await
                && resp.status().is_success()
            {
                let json: Value = resp.json().await.unwrap_or_default();
                let models = extract_models_from_response(
                    &provider.protocol,
                    provider.vendor.as_deref(),
                    &json,
                );
                if !models.is_empty() {
                    return Ok(merge_model_lists(models, preset_extra_models(&provider)));
                }
            }
        }

        let static_list = parse_static_models(provider.static_models.as_deref());
        let extra = preset_extra_models(&provider);
        if !extra.is_empty() {
            return Ok(merge_model_lists(static_list, extra));
        }
        Ok(static_list)
    }

    pub async fn get_model_capabilities(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let provider = self.get_provider(provider_id).await?;
        let trimmed_model = model.trim();
        if trimmed_model.is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        self.resolve_provider_model_capabilities(&provider, trimmed_model)
            .await
    }

    async fn resolve_provider_model_capabilities(
        &self,
        provider: &Provider,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let mut caps = match preset_capabilities_source(provider) {
            CapabilitiesSource::ModelsDev(vendor_key) => {
                let matched =
                    lookup_models_dev_capability(&self.gw.config.data_dir, vendor_key, model);
                matched.ok_or_else(|| {
                    anyhow::anyhow!("no matched model capabilities found in models.dev")
                })?
            }
            CapabilitiesSource::Http(url) => {
                if is_ollama_show_endpoint(url) {
                    self.query_ollama_show_capability(url, model).await
                } else {
                    self.query_http_capability(provider, url, model).await
                }?
            }
            CapabilitiesSource::Auto => fuzzy_match_models_dev(&self.gw.config.data_dir, model)
                .ok_or_else(|| {
                    anyhow::anyhow!("no matched model capabilities found in auto mode")
                })?,
        };
        // bigmodel.cn（人民币计费站）官方牌价覆盖目录 USD 折算。
        super::model_catalog::apply_bigmodel_cn_official_pricing(&mut caps, &provider.base_url);
        Ok(caps)
    }

    async fn query_http_capability(
        &self,
        provider: &Provider,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let runtime = self.resolve_provider_runtime(provider).await?;
        let credential = runtime.access_token;
        let mut headers = if runtime.binding.disable_default_auth {
            HeaderMap::new()
        } else {
            build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?
        };
        headers.extend(runtime_binding_headers(&runtime.binding)?);
        let mut request = self
            .gw
            .http_client
            .get(url)
            .headers(headers)
            .timeout(Duration::from_secs(10));

        if is_google_protocol(&provider.protocol) && !runtime.binding.disable_default_auth {
            let separator = if url.contains('?') { '&' } else { '?' };
            let mut headers =
                build_model_headers(&provider.protocol, provider.vendor.as_deref(), &credential)?;
            headers.extend(runtime_binding_headers(&runtime.binding)?);
            request = self
                .gw
                .http_client
                .get(format!("{url}{separator}key={}", credential))
                .headers(headers)
                .timeout(Duration::from_secs(10));
        }

        let resp = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("capability source returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        if let Some(cap) = parse_http_capability(&json, model) {
            return Ok(cap);
        }
        anyhow::bail!("no matched model capabilities found from capability source")
    }

    async fn query_ollama_show_capability(
        &self,
        url: &str,
        model: &str,
    ) -> anyhow::Result<ModelCapabilities> {
        let resp = self
            .gw
            .http_client
            .post(url)
            .json(&serde_json::json!({ "name": model }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(format_connectivity_error(&e)))?;
        if !resp.status().is_success() {
            anyhow::bail!("ollama /api/show returned status {}", resp.status());
        }
        let json: Value = resp.json().await.unwrap_or_default();
        Ok(parse_ollama_capability(&json, model))
    }
}

#[cfg(test)]
mod probe_reply_tests {
    use super::*;

    #[test]
    fn gemini_plain_reply_extracts_text() {
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hey from gemini"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1}}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("hey from gemini")
        );
    }

    #[test]
    fn gemini_antigravity_envelope_reply_extracts_text() {
        let body = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"wrapped ok"}]},"finishReason":"STOP"}]},"responseId":"r1","modelVersion":"gemini-2.5-pro"}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("wrapped ok"));
    }

    #[test]
    fn gemini_reply_falls_back_to_thought_parts() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"deep thought","thought":true}]},"finishReason":"STOP"}]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] deep thought")
        );
    }

    #[test]
    fn gemini_finished_but_silent_counts_as_completed() {
        // The Code Assist non-stream surface occasionally returns a finished
        // envelope with no text; HTTP 200 + finishReason proves callability.
        let body = r#"{"response":{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3}}}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn antigravity_probe_request_builds_v1internal_envelope() {
        let mut runtime_headers = HeaderMap::new();
        runtime_headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer ya29.probe"));
        let (url, headers, body) = build_gemini_model_probe_request(
            "https://cloudcode-pa.googleapis.com",
            "ya29.probe",
            &runtime_headers,
            "gemini-2.5-pro",
            Some("antigravity"),
            Some("cloudaicompanion-9"),
        )
        .unwrap();
        assert_eq!(
            url,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer ya29.probe");
        assert_eq!(body["model"], "gemini-2.5-pro");
        assert_eq!(body["project"], "cloudaicompanion-9");
        assert!(body["request"]["contents"].is_array());
        assert!(body["request"]["systemInstruction"]["parts"].is_array());
    }

    #[test]
    fn antigravity_probe_request_without_project_fails() {
        let error = build_gemini_model_probe_request(
            "https://cloudcode-pa.googleapis.com",
            "ya29.probe",
            &HeaderMap::new(),
            "gemini-2.5-pro",
            Some("antigravity"),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("project_id"));
    }

    #[test]
    fn google_default_probe_request_uses_query_key() {
        let (url, _headers, body) = build_gemini_model_probe_request(
            "https://generativelanguage.googleapis.com",
            "gem-key",
            &HeaderMap::new(),
            "gemini-2.5-flash",
            Some("default"),
            None,
        )
        .unwrap();
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse?key=gem-key"
        );
        assert!(body.get("contents").is_some());
        assert!(body.get("request").is_none());
    }

    #[test]
    fn gemini_sse_stream_accumulates_plain_chunks() {
        let body = concat!(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#,
            "\n\n",
            r#"data: {"candidates":[{"content":{"parts":[{"text":"lo"}]}}]}"#,
            "\n\n",
            r#"data: {"candidates":[{"content":{"parts":[{"text":"!"}]},"finishReason":"STOP"}],"usageMetadata":{"candidatesTokenCount":3}}"#,
            "\n\n",
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("Hello!"));
    }

    #[test]
    fn gemini_sse_stream_unwraps_antigravity_envelope() {
        let body = concat!(
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"stream"}]}}]},"responseId":"r1"}"#,
            "\n\n",
            r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":" ok"}]},"finishReason":"STOP"}]}}"#,
            "\n\n",
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("stream ok"));
    }

    #[test]
    fn gemini_sse_stream_falls_back_to_thought_parts() {
        let body = concat!(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"pondering","thought":true}]},"finishReason":"STOP"}]}"#,
            "\n\n",
        );
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] pondering")
        );
    }

    #[test]
    fn gemini_sse_stream_without_terminal_is_not_a_success() {
        let body = concat!(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"cut off"}]}}]}"#,
            "\n\n",
        );
        // Falls through to JSON extraction, which fails on an SSE body.
        assert_eq!(extract_probe_reply(body), None);
    }
    #[test]
    fn openai_chat_reply_prefers_content() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello","reasoning_content":"thought"}}]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello"));
    }

    #[test]
    fn openai_chat_falls_back_to_reasoning_when_content_empty() {
        // Reasoning model whose visible text was cut by the token budget.
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"","reasoning_content":"Let me think"}}]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] Let me think")
        );
    }

    #[test]
    fn openai_responses_text_parts_win() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"hmm"}]},
            {"type":"message","content":[{"type":"output_text","text":"hi there"}]}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hi there"));
    }

    #[test]
    fn openai_responses_falls_back_to_reasoning_summary() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"pondering"}]}
        ]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] pondering")
        );
    }

    #[test]
    fn anthropic_text_blocks_win() {
        let body = r#"{"content":[
            {"type":"thinking","thinking":"internal"},
            {"type":"text","text":"answer"}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn anthropic_falls_back_to_thinking_block() {
        let body = r#"{"content":[
            {"type":"thinking","thinking":"budget exhausted mid-thought"}
        ]}"#;
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] budget exhausted mid-thought")
        );
    }

    #[test]
    fn no_text_anywhere_is_none() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":""}}]}"#;
        assert_eq!(extract_probe_reply(body), None);
        let body = r#"{"content":[]}"#;
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn openai_responses_walks_all_content_parts_and_text_values() {
        let body = r#"{"output":[
            {"type":"reasoning","summary":[{"type":"summary_text","text":"internal"}]},
            {"type":"message","content":[
                {"type":"output_text","text":""},
                {"type":"output_text","text":{"value":"hello from sol"}}
            ]}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello from sol"));
    }

    #[test]
    fn codex_sse_accepts_output_text_done_without_a_delta() {
        let body = concat!(
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"text\":{\"value\":\"hello from sol\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello from sol"));
    }

    #[test]
    fn codex_sse_uses_payload_type_and_multiline_crlf_data() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.done\",\"text\":\"hello\"}\r\n\r\n",
            "data: {\"response\":{\"status\":\"completed\",\r\n",
            "data: \"output\":[]}}\r\n\r\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello"));
    }

    #[test]
    fn codex_sse_reasoning_text_alias_is_a_fallback() {
        let body = concat!(
            "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"pondering\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
        );
        assert_eq!(
            extract_probe_reply(body).as_deref(),
            Some("[thinking] pondering")
        );
    }

    #[test]
    fn codex_sse_output_item_done_can_carry_text_after_empty_parts() {
        let body = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"\"},{\"type\":\"output_text\",\"text\":{\"value\":\"answer\"}}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn codex_sse_does_not_duplicate_output_text_done_and_item_done() {
        let body = concat!(
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"text\":\"answer\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn incomplete_terminal_event_is_not_a_success() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn first_terminal_event_wins_over_late_failure() {
        let body = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn failure_before_completion_is_not_a_success() {
        let body = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn visible_deltas_beat_conflicting_terminal_text() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"delta answer\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output_text\":\"different answer\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("delta answer"));
    }

    #[test]
    fn nonstream_incomplete_response_is_not_a_success() {
        let body = r#"{"status":"incomplete","output":[
            {"type":"message","content":[{"type":"output_text","text":"partial"}]}
        ]}"#;
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn empty_output_text_falls_back_to_typed_output_parts() {
        let body = r#"{"status":"completed","output_text":"","output":[
            {"type":"message","content":[{"type":"output_text","text":"answer"}]}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn streamed_delta_fragments_preserve_interword_whitespace() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("hello world"));
    }

    #[test]
    fn tool_reasoning_fields_do_not_become_probe_text() {
        let body = r#"{"status":"completed","output":[
            {"type":"function_call","name":"exec","reasoning":"internal"},
            {"type":"custom_tool_call","name":"wait","text":"internal"}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn tool_only_responses_do_not_become_visible_text() {
        let body = r#"{"status":"completed","output":[
            {"type":"function_call","name":"exec","arguments":"run"},
            {"type":"custom_tool_call","name":"wait","input":"later"}
        ]}"#;
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn whitespace_only_delta_falls_back_to_terminal_text() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"   \"}\n\n",
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"text\":\"answer\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn unterminated_final_sse_event_is_processed() {
        let body = concat!(
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.done\",\"text\":\"answer\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}"
        );
        assert_eq!(extract_probe_reply(body).as_deref(), Some("answer"));
    }

    #[test]
    fn malformed_terminal_payload_is_not_a_success() {
        let body = "event: response.completed\ndata: {not-json}\n\n";
        assert_eq!(extract_probe_reply(body), None);
    }

    #[test]
    fn oauth_runtime_headers_authoritatively_authenticate_model_probe() {
        let runtime_headers = HeaderMap::from_iter([
            (
                AUTHORIZATION,
                HeaderValue::from_static("Bearer oauth-access-token"),
            ),
            (
                reqwest::header::HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_static("account-1"),
            ),
            (
                reqwest::header::HeaderName::from_static("originator"),
                HeaderValue::from_static("Codex Desktop"),
            ),
        ]);
        let (url, headers, body) = build_model_probe_request(
            crate::protocol::ids::Protocol::OpenAIResponses,
            "https://chatgpt.com/backend-api/codex",
            "",
            "none",
            &runtime_headers,
            "gpt-5-codex",
            true,
            Some("codex"),
            true,
            None,
        )
        .unwrap();

        assert_eq!(url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer oauth-access-token"
        );
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "account-1");
        assert_eq!(headers.get("originator").unwrap(), "Codex Desktop");
        assert_eq!(
            body.get("model").and_then(Value::as_str),
            Some("gpt-5-codex")
        );
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
        assert_eq!(body.get("store").and_then(Value::as_bool), Some(false));
        assert_eq!(
            body.get("service_tier").and_then(Value::as_str),
            Some("priority"),
            "Fast mode must be applied to Codex model probes",
        );
        assert_eq!(
            body.pointer("/input/0/content/0/type")
                .and_then(Value::as_str),
            Some("input_text")
        );
        assert_eq!(
            body.pointer("/input/0/content/0/text")
                .and_then(Value::as_str),
            Some("hi")
        );
    }

    #[test]
    fn codex_sse_probe_extracts_text_and_requires_completion() {
        let completed = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"he\"}\n\n",
            "data:{\"type\":\"response.output_text.delta\",\"delta\":\"llo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
        );
        assert_eq!(extract_probe_reply(completed).as_deref(), Some("hello"));

        let truncated = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        assert_eq!(extract_probe_reply(truncated), None);
    }

    #[test]
    fn codex_sse_completed_without_text_still_proves_model_is_callable() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
        assert_eq!(extract_probe_reply(body).as_deref(), Some("[completed]"));
    }

    #[test]
    fn model_probe_uses_fast_mode_for_sub2api_responses() {
        let (_, _, body) = build_model_probe_request(
            crate::protocol::ids::Protocol::OpenAIResponses,
            "https://example.com/v1",
            "sk-test",
            "bearer",
            &HeaderMap::new(),
            "gpt-test",
            true,
            Some("sub2api"),
            false,
            None,
        )
        .unwrap();

        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn model_probe_fast_mode_stays_scoped_to_responses_consumer_channels() {
        for (protocol, fast_mode, channel) in [
            (
                crate::protocol::ids::Protocol::OpenAIResponses,
                false,
                Some("sub2api"),
            ),
            (
                crate::protocol::ids::Protocol::OpenAIResponses,
                true,
                Some("default"),
            ),
            (
                crate::protocol::ids::Protocol::OpenAICompatible,
                true,
                Some("sub2api"),
            ),
        ] {
            let (_, _, body) = build_model_probe_request(
                protocol,
                "https://example.com/v1",
                "sk-test",
                "bearer",
                &HeaderMap::new(),
                "gpt-test",
                fast_mode,
                channel,
                false,
                None,
            )
            .unwrap();

            assert!(
                body.get("service_tier").is_none(),
                "protocol={protocol:?} fast_mode={fast_mode} channel={channel:?}",
            );
        }
    }

    #[test]
    fn runtime_authorization_overrides_default_probe_api_key() {
        let runtime_headers = HeaderMap::from_iter([(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer oauth-access-token"),
        )]);
        let (_, headers, _) = build_model_probe_request(
            crate::protocol::ids::Protocol::OpenAIResponses,
            "https://example.com",
            "legacy-api-key",
            "bearer",
            &runtime_headers,
            "gpt-test",
            false,
            None,
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            headers.get(AUTHORIZATION).unwrap(),
            "Bearer oauth-access-token"
        );
    }
}

#[cfg(test)]
mod adaptive_protocol_tests {
    use super::*;

    fn endpoint(protocol: &str, priority: i32) -> CreateProviderProtocolEndpoint {
        CreateProviderProtocolEndpoint {
            protocol: protocol.to_string(),
            base_url: format!("https://{}.example", protocol.split('/').nth(1).unwrap()),
            api_key: format!("key-{priority}"),
            auth_scheme: "auto".to_string(),
            is_enabled: true,
            priority,
        }
    }

    #[test]
    fn adaptive_config_keeps_explicit_priorities_and_fills_zero_values() {
        let normalized = normalize_protocol_config(
            "adaptive",
            "openai-compatible/chat-completions/v1",
            "",
            "",
            "apikey",
            vec![
                endpoint("openai-compatible/chat-completions/v1", 0),
                endpoint("anthropic-messages/messages/2023-06-01", 7),
                endpoint("openai-responses/responses/v1", 0),
            ],
        )
        .unwrap();

        assert_eq!(
            normalized.default_protocol,
            "openai-compatible/chat-completions/v1"
        );
        assert_eq!(
            normalized
                .endpoints
                .iter()
                .map(|endpoint| endpoint.priority)
                .collect::<Vec<_>>(),
            vec![0, 7, 2]
        );
    }

    #[test]
    fn adaptive_config_rejects_disabled_default_endpoint() {
        let mut disabled = endpoint("openai-compatible/chat-completions/v1", 0);
        disabled.is_enabled = false;
        let default_protocol = disabled.protocol.clone();
        let error = normalize_protocol_config(
            "adaptive",
            &default_protocol,
            "",
            "",
            "apikey",
            vec![
                disabled,
                endpoint("anthropic-messages/messages/2023-06-01", 1),
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("not configured or enabled"));
    }

    #[test]
    fn adaptive_config_rejects_vertex_provider_selection() {
        let error =
            ensure_adaptive_provider_supported("adaptive", Some("vertexai"), None).unwrap_err();
        assert!(error.to_string().contains("does not support Vertex AI"));
    }
}
