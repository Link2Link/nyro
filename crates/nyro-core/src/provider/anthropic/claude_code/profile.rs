//! Claude Code CLI wire profile: request predicates, native-client detection
//! and per-request `anthropic-beta` assembly.
//!
//! Values mirror CLIProxyAPI's measured Claude Code 2.1.280 baseline
//! (`helps/claude_device_profile.go`, `claudeCodeCLIBetas`). Bump them
//! together, and only after measuring a new Claude Code release.

use std::collections::HashSet;

use reqwest::header::HeaderMap;
use serde_json::Value;

pub(crate) const CLI_VERSION: &str = "2.1.280";
pub(crate) const CLI_USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
pub(crate) const STAINLESS_PACKAGE_VERSION: &str = "0.112.1";
pub(crate) const STAINLESS_RUNTIME_VERSION: &str = "v26.3.0";
pub(crate) const STAINLESS_OS: &str = "MacOS";
pub(crate) const STAINLESS_ARCH: &str = "arm64";
pub(crate) const STAINLESS_TIMEOUT: &str = "600";

pub(crate) const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";
const REDACT_THINKING_BETA: &str = "redact-thinking-2026-02-12";
const MID_CONV_SYSTEM_BETA: &str = "mid-conversation-system-2026-04-07";
const PER_TURN_CONTROL_BETA: &str = "per-turn-control-2026-07-01";
const PER_TURN_TIMING_BETA: &str = "timing-2026-09-09";
const MID_CONV_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";
const MID_CONV_CLEAR_AT_BETA: &str = "mid-conversation-system-clear-at-2026-08-21";
const DANGEROUS_TOOL_USE_BETA: &str = "dangerous-tool-use-2026-09-03";
const ADVISOR_TOOL_BETA: &str = "advisor-tool-2026-03-01";
const ADVANCED_TOOL_USE_BETA: &str = "advanced-tool-use-2025-11-20";
const EFFORT_BETA: &str = "effort-2025-11-24";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
const FALLBACK_CREDIT_BETA: &str = "fallback-credit-2026-06-01";
const STRUCTURED_OUTPUTS_BETA: &str = "structured-outputs-2025-12-15";
const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
const THINKING_DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";
const THINKING_RESUMPTION_BETA: &str = "thinking-resumption-2026-07-17";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
const AFK_MODE_BETA: &str = "afk-mode-2026-01-31";
const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
const PROMPT_CACHING_EVICT_BETA: &str = "prompt-caching-evict-2026-05-12";
const CACHE_DIAGNOSIS_BETA: &str = "cache-diagnosis-2026-04-07";
const TOKEN_COUNTING_BETA: &str = "token-counting-2024-11-01";

/// Betas the `cli` entrypoint sends on every Messages request, in wire order.
const CLI_CONSTANT_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    REDACT_THINKING_BETA,
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
];

/// Every beta this profile assembles or gates. A cloaked caller's managed
/// betas are dropped on first-party Anthropic (they only act as capability
/// requests); unknown ones are newer-client features and are forwarded.
const MANAGED_BETAS: &[&str] = &[
    TOKEN_COUNTING_BETA,
    FAST_MODE_BETA,
    OAUTH_BETA,
    CLAUDE_CODE_BETA,
    CONTEXT_1M_BETA,
    MID_CONV_SYSTEM_BETA,
    PER_TURN_CONTROL_BETA,
    PER_TURN_TIMING_BETA,
    MID_CONV_TOOL_CHANGES_BETA,
    INLINE_TOOLS_BETA,
    MID_CONV_CLEAR_AT_BETA,
    DANGEROUS_TOOL_USE_BETA,
    ADVISOR_TOOL_BETA,
    ADVANCED_TOOL_USE_BETA,
    EFFORT_BETA,
    SERVER_SIDE_FALLBACK_BETA,
    FALLBACK_CREDIT_BETA,
    STRUCTURED_OUTPUTS_BETA,
    THINKING_DISPLAY_UPDATES_BETA,
    THINKING_BINDING_BETA,
    THINKING_RESUMPTION_BETA,
    EXTENDED_CACHE_TTL_BETA,
    PROMPT_CACHING_EVICT_BETA,
    CACHE_DIAGNOSIS_BETA,
    REDACT_THINKING_BETA,
    AFK_MODE_BETA,
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
];

/// Official model IDs that reject a mid-conversation `role: system` turn;
/// callers' system prompts go into a `<system-reminder>` there instead.
const LEGACY_SYSTEM_REMINDER_MODELS: &[&str] = &[
    "claude-3-5-haiku-20241022",
    "claude-3-5-haiku-latest",
    "claude-3-7-sonnet-20250219",
    "claude-3-7-sonnet-latest",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-opus-4",
    "claude-opus-4-20250514",
    "claude-opus-4-1",
    "claude-opus-4-1-20250805",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4",
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
];

// ── Body predicates ──────────────────────────────────────────────────────────

fn str_at<'a>(body: &'a Value, pointer: &str) -> &'a str {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

pub(crate) fn canonical_model(body: &Value) -> String {
    let model = str_at(body, "/model").trim().to_ascii_lowercase();
    match model.rfind('/') {
        Some(slash) => model[slash + 1..].to_string(),
        None => model,
    }
}

fn model_is(model: &str, family: &str) -> bool {
    model == family
        || model.starts_with(&format!("{family}-"))
        || model.starts_with(&format!("{family}["))
}

fn is_opus_55(model: &str) -> bool {
    model == "claude-opus-5-5" || model.starts_with("claude-opus-5-5[")
}

fn is_sonnet_55(model: &str) -> bool {
    model_is(model, "claude-sonnet-5-5")
}

fn is_sonnet_5(model: &str) -> bool {
    !is_sonnet_55(model) && model_is(model, "claude-sonnet-5")
}

fn is_fable_51(model: &str) -> bool {
    ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"]
        .iter()
        .any(|target| {
            model.match_indices(target).any(|(idx, _)| {
                model
                    .as_bytes()
                    .get(idx + target.len())
                    .is_none_or(|next| !next.is_ascii_digit())
            })
        })
}

fn uses_progress_display(model: &str) -> bool {
    is_opus_55(model) || is_fable_51(model) || is_sonnet_5(model) || is_sonnet_55(model)
}

fn has_per_turn_effort(model: &str) -> bool {
    is_opus_55(model) || model.starts_with("claude-fable-5-1")
}

fn has_per_turn_timing(model: &str) -> bool {
    has_per_turn_effort(model) || model.starts_with("claude-mythos-5-1")
}

/// Whether the target model has no mid-conversation `role: system` slot.
pub(crate) fn uses_legacy_system_reminder(body: &Value) -> bool {
    LEGACY_SYSTEM_REMINDER_MODELS.contains(&canonical_model(body).as_str())
}

fn thinking_type(body: &Value) -> String {
    str_at(body, "/thinking/type").trim().to_ascii_lowercase()
}

fn thinking_display_set(body: &Value) -> bool {
    body.pointer("/thinking/display")
        .and_then(Value::as_str)
        .is_some_and(|display| !display.trim().is_empty())
}

fn messages(body: &Value) -> &[Value] {
    body.get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn tools(body: &Value) -> &[Value] {
    body.get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn tool_type(tool: &Value) -> String {
    str_at(tool, "/type").trim().to_ascii_lowercase()
}

fn content_text(content: &Value) -> Vec<&str> {
    match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Native preflight/quota probe (`max_tokens: 1`, no tools, trivial prompt).
fn is_probe(body: &Value) -> bool {
    if body.get("max_tokens").and_then(Value::as_i64) != Some(1) || !tools(body).is_empty() {
        return false;
    }
    let messages = messages(body);
    if messages.is_empty() {
        return true;
    }
    if messages.len() != 1 || str_at(&messages[0], "/role") != "user" {
        return false;
    }
    let trivial = |text: &str| matches!(text.trim(), "quota" | "test" | "." | "probe");
    match messages[0].get("content") {
        Some(Value::String(text)) => trivial(text),
        Some(Value::Array(parts)) => {
            let mut non_reminder = 0;
            let mut matched = false;
            for part in parts {
                let text = str_at(part, "/text").trim();
                if text.contains("<system-reminder>") {
                    continue;
                }
                non_reminder += 1;
                if trivial(text) || (text == "Hi" && part.get("cache_control").is_some()) {
                    matched = true;
                }
            }
            non_reminder == 1 && matched
        }
        _ => false,
    }
}

/// Native session-title helper.
fn is_title_helper(body: &Value) -> bool {
    let title_instruction = |text: &str| {
        text.contains("naming a coding session")
            || text.contains("Return a short title")
            || text.contains("Write the title in the predominant language")
    };
    let system_texts: Vec<&str> = body.get("system").map(content_text).unwrap_or_default();
    if let Some(props) = body
        .pointer("/output_config/format/schema/properties")
        .and_then(Value::as_object)
    {
        if props.len() != 1 || !props.contains_key("title") {
            return false;
        }
        let in_messages = messages(body).iter().any(|message| {
            message
                .get("content")
                .map(content_text)
                .unwrap_or_default()
                .into_iter()
                .any(|text| title_instruction(text) || text.contains("<session>"))
        });
        return system_texts
            .iter()
            .any(|text| title_instruction(text) || text.contains("<session>"))
            || in_messages;
    }
    system_texts.iter().any(|text| title_instruction(text))
        || messages(body).iter().any(|message| {
            str_at(message, "/role") == "system"
                && message
                    .get("content")
                    .map(content_text)
                    .unwrap_or_default()
                    .into_iter()
                    .any(title_instruction)
        })
}

pub(crate) fn is_probe_or_helper(body: &Value) -> bool {
    is_probe(body) || is_title_helper(body)
}

/// Claude Code subagent traffic (header or body markers).
pub(crate) fn is_subagent(headers: &HeaderMap, body: &Value) -> bool {
    if ["x-claude-code-agent-id", "x-claude-code-parent-agent-id"]
        .iter()
        .any(|name| header_str(headers, name).is_some_and(|v| !v.is_empty()))
    {
        return true;
    }
    if str_at(body, "/metadata/user_id").contains("\"parent_session_id\"") {
        return true;
    }
    match body.get("system") {
        Some(Value::Array(blocks)) => blocks
            .first()
            .is_some_and(|block| str_at(block, "/text").contains("cc_is_subagent=true")),
        Some(Value::String(text)) => text.contains("cc_is_subagent=true"),
        _ => false,
    }
}

/// Whether any `cache_control` block asks for the 1h TTL.
pub(crate) fn has_1h_ttl(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("cache_control")
                .and_then(|cc| cc.get("ttl"))
                .and_then(Value::as_str)
                == Some("1h")
                || map.values().any(has_1h_ttl)
        }
        Value::Array(items) => items.iter().any(has_1h_ttl),
        _ => false,
    }
}

fn supports_effort(body: &Value) -> bool {
    !is_probe_or_helper(body)
        && !canonical_model(body).contains("haiku")
        && thinking_type(body) != "disabled"
}

fn uses_advanced_tool_use(body: &Value) -> bool {
    tools(body).iter().any(|tool| {
        tool_type(tool).starts_with("tool_search_tool_")
            || tool.get("defer_loading").and_then(Value::as_bool) == Some(true)
            || tool.get("input_examples").is_some()
            || tool.get("allowed_callers").is_some()
    })
}

fn has_advisor_tool(body: &Value) -> bool {
    tools(body)
        .iter()
        .any(|tool| tool_type(tool).starts_with("advisor_"))
}

fn has_inline_tool_addition(body: &Value) -> bool {
    messages(body).iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    str_at(part, "/type").eq_ignore_ascii_case("tool_addition")
                        && part.pointer("/tool/definition").is_some()
                })
            })
    })
}

fn has_per_turn_timing_body(body: &Value) -> bool {
    body.pointer("/output_config/timing").is_some()
        || messages(body)
            .iter()
            .any(|message| message.pointer("/output_config/timing").is_some())
}

// ── Beta assembly ────────────────────────────────────────────────────────────

/// Split a comma-separated beta header value into trimmed, non-empty names.
pub(crate) fn split_betas(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|beta| !beta.is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Default)]
struct BetaList {
    items: Vec<String>,
}

impl BetaList {
    fn from(items: Vec<String>) -> Self {
        let mut list = Self::default();
        for item in items {
            list.push(&item);
        }
        list
    }
    fn contains(&self, beta: &str) -> bool {
        self.items.iter().any(|item| item == beta)
    }
    fn push(&mut self, beta: &str) {
        if !beta.is_empty() && !self.contains(beta) {
            self.items.push(beta.to_string());
        }
    }
    fn insert_after_claude_code(&mut self, beta: &str) {
        if self.contains(beta) {
            return;
        }
        let at = usize::from(self.items.first().map(String::as_str) == Some(CLAUDE_CODE_BETA));
        self.items.insert(at, beta.to_string());
    }
    fn remove(&mut self, beta: &str) {
        self.items.retain(|item| item != beta);
    }
    fn join(&self) -> String {
        self.items.join(",")
    }
}

/// The per-request beta baseline Claude Code 2.1.280 sends (`claudeCodeCLIBetas`).
fn cli_betas(body: &Value, requested: &HashSet<String>, oauth: bool) -> BetaList {
    let req = |beta: &str| requested.contains(beta);
    let model = canonical_model(body);
    let legacy = uses_legacy_system_reminder(body);
    let probe = is_probe_or_helper(body);
    let thinking = thinking_type(body);
    let mut betas = BetaList::default();

    betas.push(CLAUDE_CODE_BETA);
    if oauth {
        betas.push(OAUTH_BETA);
    }
    if req(CONTEXT_1M_BETA) {
        betas.push(CONTEXT_1M_BETA);
    }
    let redact = !thinking_display_set(body);
    for beta in CLI_CONSTANT_BETAS {
        if *beta == REDACT_THINKING_BETA && !redact {
            continue;
        }
        betas.push(beta);
    }
    let per_turn_control = req(PER_TURN_CONTROL_BETA) || has_per_turn_effort(&model);
    let per_turn_timing = req(PER_TURN_TIMING_BETA)
        || (has_per_turn_timing(&model) && has_per_turn_timing_body(body));
    if !legacy {
        betas.push(MID_CONV_SYSTEM_BETA);
        if per_turn_control {
            betas.push(PER_TURN_CONTROL_BETA);
        }
        if per_turn_timing {
            betas.push(PER_TURN_TIMING_BETA);
        }
        if !is_sonnet_5(&model) {
            betas.push(MID_CONV_TOOL_CHANGES_BETA);
        }
        if req(INLINE_TOOLS_BETA) || has_inline_tool_addition(body) {
            betas.push(INLINE_TOOLS_BETA);
        }
    } else {
        if per_turn_control {
            betas.push(PER_TURN_CONTROL_BETA);
        }
        if per_turn_timing {
            betas.push(PER_TURN_TIMING_BETA);
        }
    }
    if req(ADVISOR_TOOL_BETA) || has_advisor_tool(body) {
        betas.push(ADVISOR_TOOL_BETA);
    }
    if req(ADVANCED_TOOL_USE_BETA) || uses_advanced_tool_use(body) {
        betas.push(ADVANCED_TOOL_USE_BETA);
    }
    let clear_at = req(MID_CONV_CLEAR_AT_BETA)
        || uses_progress_display(&model)
        || messages(body).iter().any(|m| m.get("clear_at").is_some());
    if !legacy && clear_at {
        betas.push(MID_CONV_CLEAR_AT_BETA);
    }
    if req(DANGEROUS_TOOL_USE_BETA) || body.get("safeguards").is_some() {
        betas.push(DANGEROUS_TOOL_USE_BETA);
    }
    if supports_effort(body) {
        betas.push(EFFORT_BETA);
    }
    let has_fallbacks = body.get("fallbacks").is_some();
    if !probe && (req(SERVER_SIDE_FALLBACK_BETA) || has_fallbacks) {
        betas.push(SERVER_SIDE_FALLBACK_BETA);
    }
    if req(FALLBACK_CREDIT_BETA)
        || body.get("fallback_credit_token").is_some()
        || (oauth && has_fallbacks)
    {
        betas.push(FALLBACK_CREDIT_BETA);
    }
    if req(STRUCTURED_OUTPUTS_BETA) {
        betas.push(STRUCTURED_OUTPUTS_BETA);
    }
    if req(THINKING_BINDING_BETA)
        || body.pointer("/thinking/block_binding").is_some()
        || (uses_progress_display(&model) && thinking == "adaptive")
    {
        betas.push(THINKING_BINDING_BETA);
    }
    let display_updates = body
        .pointer("/thinking/display")
        .and_then(Value::as_str)
        .is_some_and(|d| d.trim().eq_ignore_ascii_case("updates"));
    if !probe && thinking != "disabled" && (req(THINKING_DISPLAY_UPDATES_BETA) || display_updates) {
        betas.push(THINKING_DISPLAY_UPDATES_BETA);
    }
    if req(THINKING_RESUMPTION_BETA) {
        betas.push(THINKING_RESUMPTION_BETA);
    }
    let fast = str_at(body, "/speed").trim().eq_ignore_ascii_case("fast");
    if req(FAST_MODE_BETA) || fast {
        betas.push(FAST_MODE_BETA);
    }
    if req(AFK_MODE_BETA) {
        betas.push(AFK_MODE_BETA);
    }
    if !probe
        && ((oauth && !is_subagent(&HeaderMap::new(), body))
            || req(EXTENDED_CACHE_TTL_BETA)
            || has_1h_ttl(body))
    {
        betas.push(EXTENDED_CACHE_TTL_BETA);
    }
    if req(PROMPT_CACHING_EVICT_BETA)
        || serde_json::to_string(body).is_ok_and(|raw| raw.contains("\"evict_on_complete\""))
    {
        betas.push(PROMPT_CACHING_EVICT_BETA);
    }
    if body.get("diagnostics").is_some_and(Value::is_object) {
        betas.push(CACHE_DIAGNOSIS_BETA);
    }
    betas
}

/// Gating every profile applies last, cloaked and native alike.
fn apply_post_filters(betas: &mut BetaList, body: &Value, headers: &HeaderMap) {
    if !supports_effort(body) {
        betas.remove(EFFORT_BETA);
    }
    let probe = is_probe_or_helper(body);
    if probe {
        betas.remove(SERVER_SIDE_FALLBACK_BETA);
        betas.remove(THINKING_DISPLAY_UPDATES_BETA);
        betas.remove(EXTENDED_CACHE_TTL_BETA);
    }
    if thinking_type(body) == "disabled" {
        betas.remove(THINKING_DISPLAY_UPDATES_BETA);
    }
    let one_hour = has_1h_ttl(body);
    if is_subagent(headers, body) && !one_hour {
        betas.remove(EXTENDED_CACHE_TTL_BETA);
    }
    if !probe && one_hour {
        betas.push(EXTENDED_CACHE_TTL_BETA);
    }
    if canonical_model(body).contains("haiku") && body.get("fallbacks").is_none() {
        betas.remove(SERVER_SIDE_FALLBACK_BETA);
    }
    if body.pointer("/thinking/block_binding").is_some() {
        betas.push(THINKING_BINDING_BETA);
    }
}

/// `anthropic-beta` for a cloaked (non-Claude-Code) caller.
///
/// `caller_betas` are the caller's header betas, `body_betas` any `betas`
/// lifted out of the body. Both act as capability requests; on first-party
/// Anthropic only unmanaged names are forwarded verbatim.
pub(crate) fn cloaked_betas(
    body: &Value,
    caller_betas: &[String],
    body_betas: &[String],
    oauth: bool,
    first_party: bool,
    headers: &HeaderMap,
) -> String {
    let requested: HashSet<String> = caller_betas.iter().chain(body_betas).cloned().collect();
    let mut betas = cli_betas(body, &requested, oauth);
    for beta in caller_betas {
        if first_party && MANAGED_BETAS.contains(&beta.as_str()) {
            continue;
        }
        betas.push(beta);
    }
    if !first_party {
        for beta in body_betas {
            betas.push(beta);
        }
    }
    apply_post_filters(&mut betas, body, headers);
    betas.join()
}

/// `anthropic-beta` for a confirmed native Claude Code caller: its own list
/// is authoritative, plus the credential-scoped OAuth betas it cannot know
/// the gateway will need (`withClaudeOAuthCredentialBetas`).
pub(crate) fn native_betas(
    body: &Value,
    caller_betas: &[String],
    oauth: bool,
    headers: &HeaderMap,
) -> String {
    let mut betas = BetaList::from(caller_betas.to_vec());
    if has_advisor_tool(body) {
        betas.push(ADVISOR_TOOL_BETA);
    }
    if oauth {
        betas.insert_after_claude_code(OAUTH_BETA);
        let subagent = is_subagent(headers, body);
        if !is_probe_or_helper(body) && (!subagent || has_1h_ttl(body)) {
            betas.push(EXTENDED_CACHE_TTL_BETA);
        }
    }
    apply_post_filters(&mut betas, body, headers);
    betas.join()
}

// ── Native client detection ──────────────────────────────────────────────────

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// `claude-cli/<major>.<minor>.<patch> (external, <entrypoint>[, agent-sdk/x.y.z])`.
pub(crate) fn parse_cli_user_agent(ua: &str) -> Option<(u32, u32, u32, String)> {
    let rest = ua.trim().strip_prefix("claude-cli/")?;
    let (version, tail) = rest.split_once(char::is_whitespace)?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let inner = tail.trim().strip_prefix('(')?.strip_suffix(')')?;
    let mut fields = inner.split(',').map(str::trim);
    if fields.next()? != "external" {
        return None;
    }
    let entrypoint = fields.next()?.to_string();
    if entrypoint.is_empty() {
        return None;
    }
    if let Some(extra) = fields.next()
        && !extra.starts_with("agent-sdk/")
    {
        return None;
    }
    Some((major, minor, patch, entrypoint))
}

/// Whether a caller UA is a Claude Code release at or above the baseline in
/// the same major/minor line, so its measured identity may be kept.
pub(crate) fn caller_user_agent_is_current(ua: &str) -> bool {
    let Some((major, minor, patch, _)) = parse_cli_user_agent(ua) else {
        return false;
    };
    let Some((b_major, b_minor, b_patch, _)) = parse_cli_user_agent(CLI_USER_AGENT) else {
        return false;
    };
    major == b_major && minor == b_minor && patch >= b_patch
}

/// Result of inspecting the downstream request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NativeDetection {
    /// The caller is genuine Claude Code: keep its body and identity.
    pub confirmed: bool,
    /// `cc_entrypoint` advertised by the caller UA (`cli`, `sdk-cli`, …).
    pub entrypoint: Option<String>,
}

const NATIVE_ENTRYPOINTS: &[&str] = &["cli", "sdk-cli", "sdk-ts", "claude-vscode"];

/// Detect a native Claude Code client (`helps/claude_client_detection.go`):
/// `x-app: cli`, a `claude-cli/…` UA from a native entrypoint and the
/// `claude-code-20250219` beta, plus either the 2.1.220+ JSON
/// `metadata.user_id` shape or a count_tokens call.
pub(crate) fn detect_native(
    headers: &HeaderMap,
    body: &Value,
    count_tokens: bool,
) -> NativeDetection {
    let entrypoint = header_str(headers, "user-agent")
        .and_then(parse_cli_user_agent)
        .map(|(_, _, _, entrypoint)| entrypoint);
    let native_entrypoint = entrypoint
        .as_deref()
        .is_some_and(|e| NATIVE_ENTRYPOINTS.contains(&e));
    let x_app = header_str(headers, "x-app").is_some_and(|v| v.eq_ignore_ascii_case("cli"));
    let beta = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(split_betas)
        .any(|b| b == CLAUDE_CODE_BETA);
    let user_id = str_at(body, "/metadata/user_id");
    let confirmed = native_entrypoint
        && x_app
        && beta
        && (count_tokens || super::identity::is_valid_user_id(user_id));
    NativeDetection {
        confirmed,
        entrypoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    #[test]
    fn oauth_opus5_baseline_matches_cliproxyapi() {
        let body = json!({"model": "claude-opus-5"});
        let betas = cloaked_betas(&body, &[], &[], true, true, &HeaderMap::new());
        assert_eq!(
            betas,
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,mid-conversation-tool-changes-2026-07-01,\
             effort-2025-11-24,extended-cache-ttl-2025-04-11"
        );
    }

    #[test]
    fn legacy_models_skip_mid_conversation_betas_and_haiku_skips_effort() {
        let body = json!({"model": "claude-haiku-4-5-20251001"});
        let betas = cloaked_betas(&body, &[], &[], true, true, &HeaderMap::new());
        assert!(!betas.contains(MID_CONV_SYSTEM_BETA));
        assert!(!betas.contains(EFFORT_BETA));
        assert!(betas.starts_with("claude-code-20250219,oauth-2025-04-20,"));
    }

    #[test]
    fn caller_betas_managed_dropped_unknown_forwarded_on_first_party() {
        let body = json!({"model": "claude-sonnet-4-6", "thinking": {"type": "enabled", "display": "summarized"}});
        let caller = split_betas(
            "context-1m-2025-08-07, fine-grained-tool-streaming-2025-05-14 ,token-counting-2024-11-01",
        );
        let betas = cloaked_betas(
            &body,
            &caller,
            &["files-api-2025-04-14".into()],
            true,
            true,
            &HeaderMap::new(),
        );
        // context-1m is a managed capability request placed at position 3.
        assert!(betas.starts_with("claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,"));
        assert!(
            !betas.contains(REDACT_THINKING_BETA),
            "display set drops redact-thinking"
        );
        assert!(betas.ends_with(",fine-grained-tool-streaming-2025-05-14"));
        assert!(!betas.contains(TOKEN_COUNTING_BETA));
        assert!(
            !betas.contains("files-api-2025-04-14"),
            "body betas stay off first-party"
        );
    }

    #[test]
    fn fast_mode_and_probe_gating() {
        let fast = json!({"model": "claude-opus-4-7", "speed": "fast"});
        assert!(
            cloaked_betas(&fast, &[], &[], true, true, &HeaderMap::new()).contains(FAST_MODE_BETA)
        );
        let probe = json!({"model": "claude-opus-4-7", "max_tokens": 1, "messages": [{"role": "user", "content": "quota"}]});
        let betas = cloaked_betas(&probe, &[], &[], true, true, &HeaderMap::new());
        assert!(!betas.contains(EFFORT_BETA));
        assert!(!betas.contains(EXTENDED_CACHE_TTL_BETA));
    }

    #[test]
    fn native_betas_add_oauth_after_claude_code_and_extended_ttl() {
        let body =
            json!({"model": "claude-opus-4-7", "messages": [{"role": "user", "content": "hi"}]});
        let caller =
            split_betas("claude-code-20250219,interleaved-thinking-2025-05-14,effort-2025-11-24");
        assert_eq!(
            native_betas(&body, &caller, true, &HeaderMap::new()),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,effort-2025-11-24,extended-cache-ttl-2025-04-11"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-1"),
        );
        assert!(!native_betas(&body, &caller, true, &headers).contains(EXTENDED_CACHE_TTL_BETA));
    }

    #[test]
    fn user_agent_parsing() {
        assert_eq!(
            parse_cli_user_agent("claude-cli/2.1.281 (external, sdk-cli)"),
            Some((2, 1, 281, "sdk-cli".to_string()))
        );
        assert!(
            parse_cli_user_agent("claude-cli/2.1.281 (external, cli, agent-sdk/0.2.1)").is_some()
        );
        assert!(parse_cli_user_agent("claude-cli/1.0.98 (external, cli)").is_some());
        assert!(parse_cli_user_agent("curl/8.0").is_none());
        assert!(caller_user_agent_is_current(
            "claude-cli/2.1.290 (external, cli)"
        ));
        assert!(!caller_user_agent_is_current(
            "claude-cli/2.1.100 (external, cli)"
        ));
        assert!(!caller_user_agent_is_current(
            "claude-cli/2.2.0 (external, cli)"
        ));
    }

    #[test]
    fn detects_native_claude_code() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            HeaderValue::from_static("claude-cli/2.1.280 (external, cli)"),
        );
        headers.insert("x-app", HeaderValue::from_static("cli"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,oauth-2025-04-20"),
        );
        let user_id = json!({
            "device_id": "a".repeat(64),
            "account_uuid": "",
            "session_id": "8f14e45f-ceea-4672-9b8e-3c1c1a0e4a1d"
        })
        .to_string();
        let body = json!({"metadata": {"user_id": user_id}});
        let detection = detect_native(&headers, &body, false);
        assert!(detection.confirmed);
        assert_eq!(detection.entrypoint.as_deref(), Some("cli"));

        // Missing the identity shape is not enough on /v1/messages…
        assert!(!detect_native(&headers, &json!({}), false).confirmed);
        // …but count_tokens never carries metadata.
        assert!(detect_native(&headers, &json!({}), true).confirmed);
        // A spoofed UA alone is never confirmed.
        let mut spoof = HeaderMap::new();
        spoof.insert(
            "user-agent",
            HeaderValue::from_static("claude-cli/2.1.280 (external, cli)"),
        );
        assert!(!detect_native(&spoof, &body, false).confirmed);
    }
}
