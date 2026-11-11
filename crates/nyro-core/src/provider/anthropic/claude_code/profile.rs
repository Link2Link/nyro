//! Claude Code CLI wire profile: request predicates, native-client detection
//! and per-request `anthropic-beta` assembly.
//!
//! Generated identity and body-derived betas follow CLIProxyAPI's measured
//! 2.1.321 profile (b4e7dbd2 and its current capability gates). Native clients
//! retain their own identity and beta ordering independently of this profile.

use std::collections::HashSet;

use reqwest::header::HeaderMap;
use serde_json::Value;

/// A generated client is one measured profile, never independently bumped
/// CLI, SDK and runtime defaults. Native compatibility is deliberately separate.
struct GeneratedProfile {
    cli_version: &'static str,
    user_agent: &'static str,
    package_version: &'static str,
    runtime_version: &'static str,
    os: &'static str,
    arch: &'static str,
    timeout: &'static str,
}

// concat! requires a literal: bind the billing version and UA to that same input.
macro_rules! generated_profile {
    ($cli:literal, $sdk:literal, $runtime:literal) => {
        GeneratedProfile {
            cli_version: $cli,
            user_agent: concat!("claude-cli/", $cli, " (external, cli)"),
            package_version: $sdk,
            runtime_version: $runtime,
            os: "MacOS",
            arch: "arm64",
            timeout: "600",
        }
    };
}

const GENERATED_PROFILE: GeneratedProfile = generated_profile!("2.1.321", "0.128.0", "v26.12.1");

// Keep the aliases used by egress, billing and usage requests.
pub(crate) const CLI_VERSION: &str = GENERATED_PROFILE.cli_version;
pub(crate) const CLI_USER_AGENT: &str = GENERATED_PROFILE.user_agent;
pub(crate) const STAINLESS_PACKAGE_VERSION: &str = GENERATED_PROFILE.package_version;
pub(crate) const STAINLESS_RUNTIME_VERSION: &str = GENERATED_PROFILE.runtime_version;
pub(crate) const STAINLESS_OS: &str = GENERATED_PROFILE.os;
pub(crate) const STAINLESS_ARCH: &str = GENERATED_PROFILE.arch;
pub(crate) const STAINLESS_TIMEOUT: &str = GENERATED_PROFILE.timeout;

// Raising the generated identity must not rewrite previously supported native
// clients with a newer header profile while preserving their older bodies.
const NATIVE_CLI_VERSION_FLOOR: (u32, u32, u32) = (2, 1, 280);

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
const THINKING_TOKEN_COUNT_BETA: &str = "thinking-token-count-2026-05-13";
const MESSAGE_THREADS_BETA: &str = "message-threads-2026-08-12";

/// Betas the `cli` entrypoint sends on every Messages request, in wire order.
const CLI_CONSTANT_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    REDACT_THINKING_BETA,
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
    THINKING_TOKEN_COUNT_BETA,
    MESSAGE_THREADS_BETA,
    "interleaved-thinking-2025-05-14",
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

fn is_fable_52(model: &str) -> bool {
    model_is(model, "claude-fable-5-2") || model_is(model, "claude-fable-5.2")
}

fn is_haiku_55(model: &str) -> bool {
    model_is(model, "claude-haiku-5-5") || model_is(model, "claude-haiku-5.5")
}

fn uses_progress_display(model: &str) -> bool {
    is_opus_55(model)
        || is_fable_51(model)
        || is_fable_52(model)
        || is_sonnet_5(model)
        || is_sonnet_55(model)
        || is_haiku_55(model)
}

fn has_per_turn_effort(model: &str) -> bool {
    is_opus_55(model)
        || model.starts_with("claude-fable-5-1")
        || is_fable_52(model)
        || is_sonnet_55(model)
        || is_haiku_55(model)
}

fn has_per_turn_timing(model: &str) -> bool {
    !is_haiku_55(model)
        && (has_per_turn_effort(model)
            || model.starts_with("claude-mythos-5-1")
            || model.starts_with("claude-mythos-5-2")
            || model.starts_with("claude-mythos-5.2"))
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

/// Inspect only API cache locations, not schemas, tool inputs/results, metadata,
/// or arbitrary nested objects that happen to contain a cache_control key.
pub(crate) fn has_1h_ttl(body: &Value) -> bool {
    let one_hour = |value: &Value| str_at(value, "/cache_control/ttl") == "1h";
    one_hour(body)
        || tools(body).iter().any(one_hour)
        || body
            .get("system")
            .and_then(Value::as_array)
            .is_some_and(|blocks| blocks.iter().any(one_hour))
        || messages(body).iter().any(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| blocks.iter().any(one_hour))
        })
}

fn supports_effort(body: &Value) -> bool {
    let model = canonical_model(body);
    !is_probe_or_helper(body)
        && (!model.contains("haiku") || is_haiku_55(&model))
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

/// The per-request beta baseline Claude Code 2.1.321 sends (`claudeCodeCLIBetas`).
fn cli_betas(body: &Value, requested: &HashSet<String>, oauth: bool) -> BetaList {
    let req = |beta: &str| requested.contains(beta);
    let model = canonical_model(body);
    let legacy = uses_legacy_system_reminder(body);
    let probe = is_probe_or_helper(body);
    let quota_probe = is_probe(body);
    let claude_code_after_scope = is_haiku_55(&model);
    let thinking = thinking_type(body);
    let mut betas = BetaList::default();

    if !quota_probe && !claude_code_after_scope {
        betas.push(CLAUDE_CODE_BETA);
    }
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
    if !quota_probe && claude_code_after_scope {
        betas.push(CLAUDE_CODE_BETA);
    }
    let per_turn_control = req(PER_TURN_CONTROL_BETA) || has_per_turn_effort(&model);
    let per_turn_timing = req(PER_TURN_TIMING_BETA)
        || (has_per_turn_timing(&model) && has_per_turn_timing_body(body));
    if !legacy {
        if req(MID_CONV_SYSTEM_BETA) {
            betas.push(MID_CONV_SYSTEM_BETA);
        }
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
    if !quota_probe || req(THINKING_TOKEN_COUNT_BETA) {
        betas.push(THINKING_TOKEN_COUNT_BETA);
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
    if supports_effort(body) {
        betas.push(EFFORT_BETA);
    }
    if req(DANGEROUS_TOOL_USE_BETA) || body.get("safeguards").is_some() {
        betas.push(DANGEROUS_TOOL_USE_BETA);
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
    if req(MESSAGE_THREADS_BETA) || body.get("thread").is_some_and(Value::is_object) {
        betas.push(MESSAGE_THREADS_BETA);
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

// ── Generated profile validation ─────────────────────────────────────────────

/// Validate the known generated wire shape after all non-native body/header
/// shaping. Callers must not use this as native-client detection or validation:
/// native releases, entrypoints and device profiles remain caller-owned.
/// Errors name only the inconsistent field, never header values or body text.
pub(crate) fn validate_generated_profile(
    headers: &HeaderMap,
    body: &Value,
    count_tokens: bool,
) -> Result<(), String> {
    for (name, expected) in [
        ("user-agent", CLI_USER_AGENT),
        ("x-stainless-package-version", STAINLESS_PACKAGE_VERSION),
        ("x-stainless-runtime-version", STAINLESS_RUNTIME_VERSION),
        ("x-stainless-runtime", "node"),
        ("x-stainless-lang", "js"),
        ("x-stainless-os", STAINLESS_OS),
        ("x-stainless-arch", STAINLESS_ARCH),
    ] {
        let mut values = headers.get_all(name).iter();
        if values.next().and_then(|value| value.to_str().ok()) != Some(expected)
            || values.next().is_some()
        {
            return Err(format!("generated Claude Code profile mismatch: {name}"));
        }
    }
    if count_tokens {
        return Ok(());
    }

    // Cloaking owns this first text block. Do not recursively inspect user
    // prompts, schemas or tool results that may quote a billing header.
    let billing = body
        .pointer("/system/0")
        .filter(|block| str_at(block, "/type") == "text")
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .and_then(|text| text.strip_prefix(super::cch::BILLING_HEADER_PREFIX))
        .ok_or_else(|| "generated Claude Code profile missing billing header".to_string())?;
    let mut versions = billing
        .split(';')
        .filter_map(|field| field.trim().strip_prefix("cc_version="));
    let version = versions.next();
    let matches_profile = version
        .and_then(|value| value.strip_prefix(CLI_VERSION))
        .and_then(|suffix| suffix.strip_prefix('.'))
        .is_some_and(|fingerprint| {
            fingerprint.len() == 3 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if !matches_profile || versions.next().is_some() {
        return Err("generated Claude Code profile mismatch: billing cc_version".to_string());
    }
    Ok(())
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

/// Whether a caller UA is in the supported native release range. This floor
/// is independent of the generated CLI identity used for non-native requests.
pub(crate) fn caller_user_agent_is_compatible(ua: &str) -> bool {
    let Some((major, minor, patch, _)) = parse_cli_user_agent(ua) else {
        return false;
    };
    let (b_major, b_minor, b_patch) = NATIVE_CLI_VERSION_FLOOR;
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

    // Native 2.1.321 main-thread captures from CLIProxyAPI, 2026-11-09.
    const CAPTURE_2321_MAIN_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,per-turn-control-2026-07-01,\
         mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15,advisor-tool-2026-03-01,\
         thinking-token-count-2026-05-13,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
         effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
         thinking-display-updates-2026-08-18,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,\
         cache-diagnosis-2026-04-07,message-threads-2026-08-12";

    fn capture_2321_main_body(model: &str, display: &str) -> Value {
        json!({
            "model": model,
            "max_tokens": 128000,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "say ok"}]},
                {"role": "system", "content": [
                    {"type": "text", "text": "tools changed"},
                    {"type": "tool_addition", "tool": {"type": "tool_definition", "definition": {
                        "name": "mcp__docs__guide", "input_schema": {"type": "object"}
                    }}}
                ]}
            ],
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "prompt", "cache_control": {
                    "type": "ephemeral", "ttl": "1h", "scope": "global"
                }}
            ],
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"name": "DeferredToolPlaceholder", "input_schema": {"type": "object"}, "defer_loading": true},
                {"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-5-5", "defer_loading": true}
            ],
            "thinking": {"type": "adaptive", "display": display},
            "safeguards": {"type": "dangerous_tool_use"},
            "output_config": {"effort": "medium"},
            "thread": {"type": "create"},
            "diagnostics": {"previous_message_id": null}
        })
    }

    #[test]
    fn generated_betas_match_claude_code_2321_captures() {
        for (model, display) in [
            ("claude-fable-5-2", "updates"),
            ("claude-fable-5-1", "updates"),
            ("claude-opus-5-5", "updates"),
            ("claude-sonnet-5-5", "updates"),
            ("claude-haiku-5-5", "updates"),
            ("claude-fable-5-2", "omitted"),
        ] {
            let body = capture_2321_main_body(model, display);
            let mut expected = CAPTURE_2321_MAIN_BETAS.to_string();
            if model == "claude-haiku-5-5" {
                expected = expected.replacen("claude-code-20250219,", "", 1).replacen(
                    "prompt-caching-scope-2026-01-05,",
                    "prompt-caching-scope-2026-01-05,claude-code-20250219,",
                    1,
                );
            }
            if display == "omitted" {
                expected = expected.replace("thinking-display-updates-2026-08-18,", "");
            }
            let requested = split_betas(&expected);
            let headers = HeaderMap::new();
            assert_eq!(
                cloaked_betas(&body, &requested, &[], true, true, &headers),
                expected,
                "{model} / {display} requested header betas"
            );
            assert_eq!(
                cloaked_betas(&body, &[], &requested, true, true, &headers),
                expected,
                "{model} / {display} requested body betas"
            );
            assert_eq!(
                cloaked_betas(&body, &[], &[], true, true, &headers),
                expected.replace("afk-mode-2026-01-31,", ""),
                "{model} / {display} inferred from body"
            );
        }
    }

    #[test]
    fn quota_probe_capture_omits_claude_code_and_thinking_token_count() {
        let body = json!({"model": "claude-haiku-4-5-20251001", "max_tokens": 1,
            "messages": [{"role": "user", "content": "quota"}]});
        let expected = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
            redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
        let headers = HeaderMap::new();
        assert_eq!(
            cloaked_betas(&body, &[], &[], true, true, &headers),
            expected
        );
        assert_eq!(
            cloaked_betas(&body, &split_betas(expected), &[], true, true, &headers),
            expected
        );
        let requested = vec![CLAUDE_CODE_BETA.into(), THINKING_TOKEN_COUNT_BETA.into()];
        assert_eq!(
            cloaked_betas(&body, &requested, &[], true, true, &headers),
            format!("{expected},{THINKING_TOKEN_COUNT_BETA}")
        );
        let helper = json!({"model": "claude-haiku-4-5-20251001", "max_tokens": 64,
            "system": "Return a short title", "messages": [{"role": "user", "content": "session"}]});
        let betas = cloaked_betas(&helper, &[], &[], true, true, &headers);
        assert!(betas.starts_with(CLAUDE_CODE_BETA));
        assert!(betas.contains(THINKING_TOKEN_COUNT_BETA));
        assert!(!betas.contains(EXTENDED_CACHE_TTL_BETA));
    }

    #[test]
    fn mid_conversation_system_and_threads_require_capabilities() {
        let mut body = json!({"model": "claude-fable-5-2",
            "messages": [{"role": "system", "content": "new instructions"}]});
        let headers = HeaderMap::new();
        let betas = cloaked_betas(&body, &[], &[], true, true, &headers);
        assert!(!betas.contains(MID_CONV_SYSTEM_BETA));
        assert!(!betas.contains(MESSAGE_THREADS_BETA));
        let requested = vec![MID_CONV_SYSTEM_BETA.into(), MESSAGE_THREADS_BETA.into()];
        let betas = cloaked_betas(&body, &requested, &[], true, true, &headers);
        assert!(betas.contains(MID_CONV_SYSTEM_BETA));
        assert!(betas.ends_with(MESSAGE_THREADS_BETA));
        body["model"] = json!("claude-haiku-4-5");
        assert!(
            !cloaked_betas(&body, &requested, &[], true, true, &headers)
                .contains(MID_CONV_SYSTEM_BETA)
        );
        for thread in [
            json!(null),
            json!("create"),
            json!([]),
            json!({"type": "create"}),
        ] {
            body["thread"] = thread.clone();
            assert_eq!(
                cloaked_betas(&body, &[], &[], true, true, &headers).contains(MESSAGE_THREADS_BETA),
                thread.is_object()
            );
        }
    }

    #[test]
    fn per_turn_timing_matches_2321_catalog_and_body_gates() {
        for (model, timing) in [
            ("claude-fable-5-2", true),
            ("provider/CLAUDE-FABLE-5-2[1m]", true),
            ("claude-fable-5.2-20261101", true),
            ("claude-fable-5-1", true),
            ("claude-opus-5-5", true),
            ("claude-sonnet-5-5", true),
            ("claude-mythos-5-2", true),
            ("claude-mythos-5.2", true),
            ("claude-haiku-5-5", false),
            ("claude-haiku-5.5", false),
            ("claude-fable-5", false),
            ("claude-fable-5-20", false),
        ] {
            let mut body = json!({"model": model});
            let headers = HeaderMap::new();
            assert!(
                !cloaked_betas(&body, &[], &[], true, true, &headers)
                    .contains(PER_TURN_TIMING_BETA)
            );
            body["output_config"] = json!({"timing": {"type": "enabled"}});
            assert_eq!(
                cloaked_betas(&body, &[], &[], true, true, &headers).contains(PER_TURN_TIMING_BETA),
                timing,
                "{model}"
            );
            body.as_object_mut().unwrap().remove("output_config");
            body["messages"] = json!([{"role": "user", "content": "hi", "output_config": {"timing": {"type": "enabled"}}}]);
            assert_eq!(
                cloaked_betas(&body, &[], &[], true, true, &headers).contains(PER_TURN_TIMING_BETA),
                timing,
                "{model}"
            );
            assert!(
                cloaked_betas(
                    &body,
                    &[PER_TURN_TIMING_BETA.into()],
                    &[],
                    true,
                    true,
                    &headers
                )
                .contains(PER_TURN_TIMING_BETA)
            );
        }
    }

    #[test]
    fn haiku_55_effort_and_progress_gates_do_not_extend_to_older_haiku() {
        for (model, supported) in [
            ("claude-haiku-5-5", true),
            ("provider/claude-haiku-5.5[1m]", true),
            ("claude-haiku-5-5-20261101", true),
            ("claude-haiku-4-5", false),
            ("claude-haiku-5-50", false),
        ] {
            let mut body = json!({"model": model, "thinking": {"type": "adaptive"}});
            let headers = HeaderMap::new();
            let betas = cloaked_betas(&body, &[], &[], true, true, &headers);
            for beta in [
                EFFORT_BETA,
                PER_TURN_CONTROL_BETA,
                MID_CONV_CLEAR_AT_BETA,
                THINKING_BINDING_BETA,
            ] {
                assert_eq!(betas.contains(beta), supported, "{model}: {beta}");
            }
            body["thinking"]["type"] = json!("disabled");
            assert!(
                !cloaked_betas(&body, &[EFFORT_BETA.into()], &[], true, true, &headers)
                    .contains(EFFORT_BETA)
            );
        }
    }

    #[test]
    fn oauth_opus5_baseline_matches_cliproxyapi() {
        let body = json!({"model": "claude-opus-5"});
        let betas = cloaked_betas(&body, &[], &[], true, true, &HeaderMap::new());
        assert_eq!(
            betas,
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-tool-changes-2026-07-01,\
             thinking-token-count-2026-05-13,effort-2025-11-24,extended-cache-ttl-2025-04-11"
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
    fn one_hour_ttl_scans_only_api_cache_positions() {
        let cache = json!({"type": "ephemeral", "ttl": "1h"});
        for body in [
            json!({"cache_control": cache}),
            json!({"tools": [{"name": "Read", "cache_control": cache}]}),
            json!({"system": [{"type": "text", "text": "prompt", "cache_control": cache}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": cache}
            ]}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "result", "cache_control": cache}
            ]}]}),
        ] {
            assert!(has_1h_ttl(&body));
        }
        for body in [
            json!({"tools": [{"input_schema": {"cache_control": cache}}]}),
            json!({"tools": [{"input_examples": [{"cache_control": cache}]}]}),
            json!({"system": [{"type": "text", "extra": {"cache_control": cache}}]}),
            json!({"messages": [{"cache_control": cache, "content": "hi"}]}),
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "input": {"cache_control": cache}}
            ]}]}),
            json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "content": [{"type": "text", "text": "nested", "cache_control": cache}]}
            ]}]}),
            json!({"messages": [{"role": "system", "content": [
                {"type": "tool_addition", "tool": {"definition": {"cache_control": cache}}}
            ]}]}),
            json!({"output_config": {"format": {"schema": {"cache_control": cache}}}}),
            json!({"metadata": {"cache_control": cache}}),
            json!({"extra": {"cache_control": cache}}),
            json!({"cache_control": {"ttl": "5m"}}),
            json!({"cache_control": {"ttl": 3600}}),
            json!({"system": {"cache_control": cache}}),
            json!({"messages": [{"content": {"cache_control": cache}}]}),
            json!(null),
        ] {
            assert!(!has_1h_ttl(&body));
        }
    }

    #[test]
    fn subagent_extended_ttl_ignores_nested_payload_cache_keys() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-1"),
        );
        let mut body = json!({"model": "claude-fable-5-2", "tools": [{
            "name": "Read", "input_schema": {"cache_control": {"ttl": "1h"}}
        }], "messages": [{"role": "user", "content": [
            {"type": "tool_result", "content": [{"cache_control": {"ttl": "1h"}}]}
        ]}]});
        let caller = vec![CLAUDE_CODE_BETA.into(), EXTENDED_CACHE_TTL_BETA.into()];
        assert!(
            !cloaked_betas(&body, &caller, &[], true, true, &headers)
                .contains(EXTENDED_CACHE_TTL_BETA)
        );
        assert!(!native_betas(&body, &caller, true, &headers).contains(EXTENDED_CACHE_TTL_BETA));
        body["messages"][0]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        assert!(
            cloaked_betas(&body, &caller, &[], true, true, &headers)
                .contains(EXTENDED_CACHE_TTL_BETA)
        );
        assert!(native_betas(&body, &caller, true, &headers).contains(EXTENDED_CACHE_TTL_BETA));
    }

    fn generated_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("user-agent", CLI_USER_AGENT),
            ("x-stainless-package-version", STAINLESS_PACKAGE_VERSION),
            ("x-stainless-runtime-version", STAINLESS_RUNTIME_VERSION),
            ("x-stainless-runtime", "node"),
            ("x-stainless-lang", "js"),
            ("x-stainless-os", STAINLESS_OS),
            ("x-stainless-arch", STAINLESS_ARCH),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        headers
    }

    fn generated_body() -> Value {
        json!({"model": "claude-fable-5-2", "system": [{"type": "text", "text": format!(
            "x-anthropic-billing-header: cc_version={CLI_VERSION}.20d; cc_entrypoint=cli; cch=00000;"
        )}]})
    }

    #[test]
    fn generated_profile_validator_accepts_messages_and_count_tokens() {
        let headers = generated_headers();
        assert_eq!(
            validate_generated_profile(&headers, &generated_body(), false),
            Ok(())
        );
        assert_eq!(
            validate_generated_profile(
                &headers,
                &json!({"model": "claude-fable-5-2", "messages": []}),
                true
            ),
            Ok(())
        );
        // Count-token payloads need no billing, including a quoted older identity.
        assert_eq!(
            validate_generated_profile(
                &headers,
                &json!({"system": "cc_version=2.1.280.abc"}),
                true
            ),
            Ok(())
        );
    }

    #[test]
    fn generated_profile_validator_rejects_header_drift_without_values() {
        let body = generated_body();
        for (name, drift) in [
            ("user-agent", "claude-cli/2.1.280 (external, cli)"),
            ("x-stainless-package-version", "0.112.1"),
            ("x-stainless-runtime-version", "v26.3.0"),
            ("x-stainless-runtime", "bun"),
            ("x-stainless-lang", "python"),
            ("x-stainless-os", "Linux"),
            ("x-stainless-arch", "x64"),
        ] {
            for count_tokens in [false, true] {
                let mut headers = generated_headers();
                headers.insert(name, HeaderValue::from_static(drift));
                headers.insert(
                    "authorization",
                    HeaderValue::from_static("Bearer secret-token"),
                );
                assert_eq!(
                    validate_generated_profile(&headers, &body, count_tokens),
                    Err(format!("generated Claude Code profile mismatch: {name}"))
                );
                headers.remove(name);
                assert!(validate_generated_profile(&headers, &body, count_tokens).is_err());
                let mut headers = generated_headers();
                headers.append(name, HeaderValue::from_static("secret-value"));
                assert!(validate_generated_profile(&headers, &body, count_tokens).is_err());
            }
        }
    }

    #[test]
    fn generated_profile_validator_checks_only_generated_billing_slot() {
        let headers = generated_headers();
        for version in [
            "2.1.280.20d",
            "2.1.321",
            "2.1.3210.20d",
            "2.1.321.zzz",
            "2.1.321.20d.secret",
            "2.1.321.20d; cc_version=2.1.280.abc",
        ] {
            let body = json!({"system": [{"type": "text", "text": format!(
                "x-anthropic-billing-header: cc_version={version}; cc_entrypoint=cli; secret-body"
            )}]});
            assert_eq!(
                validate_generated_profile(&headers, &body, false),
                Err("generated Claude Code profile mismatch: billing cc_version".to_string())
            );
        }
        let billing = generated_body()["system"][0].clone();
        for body in [
            json!({}),
            json!({"system": "x-anthropic-billing-header: cc_version=2.1.321.20d;"}),
            json!({"system": [{"type": "image", "text": billing["text"]}]}),
            json!({"system": [{"type": "text", "text": "prompt"}, billing]}),
            json!({"messages": [{"role": "user", "content": [billing]}]}),
            json!({"tools": [{"input_schema": billing}]}),
        ] {
            assert_eq!(
                validate_generated_profile(&headers, &body, false),
                Err("generated Claude Code profile missing billing header".to_string())
            );
        }
        let mut body = generated_body();
        body["messages"] = json!([{"role": "user", "content": [{"type": "text", "text":
            "x-anthropic-billing-header: cc_version=2.1.280.abc; secret-body"}]}]);
        body["system"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "text":
            "example: x-anthropic-billing-header: cc_version=2.1.280.abc;"}));
        assert_eq!(validate_generated_profile(&headers, &body, false), Ok(()));
    }

    #[test]
    fn native_betas_preserve_original_order_without_generated_baseline() {
        let body = json!({"model": "claude-fable-5-2", "thread": {"type": "create"}});
        let caller = split_betas(
            "custom-native-beta,claude-code-20250219,mid-conversation-system-2026-04-07,effort-2025-11-24",
        );
        assert_eq!(
            native_betas(&body, &caller, false, &HeaderMap::new()),
            caller.join(",")
        );
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
        assert!(caller_user_agent_is_compatible(
            "claude-cli/2.1.290 (external, cli)"
        ));
        assert!(!caller_user_agent_is_compatible(
            "claude-cli/2.1.100 (external, cli)"
        ));
        assert!(!caller_user_agent_is_compatible(
            "claude-cli/2.2.0 (external, cli)"
        ));
    }

    #[test]
    fn generated_identity_and_native_compatibility_are_independent() {
        assert_eq!(CLI_VERSION, "2.1.321");
        assert_eq!(STAINLESS_PACKAGE_VERSION, "0.128.0");
        assert_eq!(STAINLESS_RUNTIME_VERSION, "v26.12.1");
        assert_eq!(STAINLESS_OS, "MacOS");
        assert_eq!(STAINLESS_ARCH, "arm64");
        assert_eq!(
            CLI_USER_AGENT,
            format!("claude-cli/{CLI_VERSION} (external, cli)")
        );
        for version in ["2.1.280", "2.1.290", "2.1.312", "2.1.321", "2.1.322"] {
            assert!(
                caller_user_agent_is_compatible(&format!("claude-cli/{version} (external, cli)")),
                "{version}"
            );
        }
        for ua in [
            "claude-cli/2.1.279 (external, cli)",
            "claude-cli/2.2.0 (external, cli)",
            "claude-cli/3.1.321 (external, cli)",
            "curl/8.0",
        ] {
            assert!(!caller_user_agent_is_compatible(ua), "{ua}");
        }
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
