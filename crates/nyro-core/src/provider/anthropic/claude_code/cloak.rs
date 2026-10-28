//! Request-body rewriting for the Claude Code OAuth channel.
//!
//! A caller that is not genuine Claude Code is "cloaked" into the measured
//! Claude Code 2.1.280 Messages shape before it reaches Anthropic with the
//! subscription token. Ported from CLIProxyAPI `applyCloakingInternal` and the
//! surrounding body steps of `ClaudeExecutor.Execute`:
//!
//! 1. `system` becomes `[billing header, Claude Code identity]`; the caller's
//!    own system prompt moves out of the top-level slot (Anthropic's
//!    third-party classifier keys on it, CPA #6432): into mid-conversation
//!    `role: system` turns, or `<system-reminder>` text on legacy models.
//! 2. A `currentDate` reminder leads the first user turn, like the CLI.
//! 3. Sampling, forced-tool thinking, `context_management` and the
//!    `cache_control` layout are normalized to what the CLI produces.
//!
//! Native Claude Code keeps its own body; only the credential-scoped parts
//! (identity, `cch`) are touched there, see `egress`.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::cch::{BILLING_HEADER_PREFIX, CCH_PLACEHOLDER};
use super::profile::{self, canonical_model};

pub(crate) const CLAUDE_CODE_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
const AGENT_SDK_IDENTITIES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];
/// Salt Claude Code uses for the 3-char build fingerprint in `cc_version`.
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const DATE_REMINDER_PREFIX: &str = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is ";
const FABLE_REPORTING_OUTCOMES: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";
const MAX_CACHE_BREAKPOINTS: usize = 4;

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

fn text_block(text: &str, cache: bool) -> Value {
    let mut block = json!({"type": "text", "text": text});
    if cache {
        block["cache_control"] = ephemeral();
    }
    block
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn valid_cache_control(value: Option<&Value>) -> bool {
    value.and_then(|cc| cc.get("type")).and_then(Value::as_str) == Some("ephemeral")
}

pub(crate) fn is_billing_text(text: &str) -> bool {
    text.trim_start().starts_with(BILLING_HEADER_PREFIX)
}

fn is_identity_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == CLAUDE_CODE_IDENTITY || AGENT_SDK_IDENTITIES.contains(&trimmed)
}

fn is_date_reminder(text: &str) -> bool {
    text.starts_with(DATE_REMINDER_PREFIX)
}

fn is_context_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>") && text.contains("</system-reminder>")
}

pub(crate) fn date_reminder(date: &str) -> String {
    format!(
        "{DATE_REMINDER_PREFIX}{date}.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
    )
}

fn caller_system_reminder(text: &str) -> String {
    let newline = if text.ends_with('\n') { "" } else { "\n" };
    format!("<system-reminder>\n{text}{newline}</system-reminder>")
}

fn messages_mut(body: &mut Value) -> Option<&mut Vec<Value>> {
    body.get_mut("messages").and_then(Value::as_array_mut)
}

fn first_user_index(body: &Value) -> Option<usize> {
    body.get("messages")?
        .as_array()?
        .iter()
        .position(|m| str_of(m, "role") == "user")
}

// ── Billing header ───────────────────────────────────────────────────────────

/// Text the CLI fingerprints: the first user turn's string content, or its
/// first text block that is not a Claude Code reminder.
fn billing_message_text(body: &Value) -> String {
    let Some(idx) = first_user_index(body) else {
        return String::new();
    };
    match body["messages"][idx].get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| str_of(part, "type") == "text")
            .map(|part| str_of(part, "text"))
            .find(|text| !is_date_reminder(text) && !is_context_reminder(text))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// `SHA256(salt + text[4] + text[7] + text[20] + version)[:3]`, sampling
/// UTF-16 code units like the JS client (missing positions read as `'0'`).
pub(crate) fn build_fingerprint(message_text: &str, version: &str) -> String {
    let units: Vec<u16> = message_text.encode_utf16().collect();
    let sampled: Vec<u16> = [4usize, 7, 20]
        .iter()
        .map(|&idx| units.get(idx).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let sampled: String = char::decode_utf16(sampled)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    let digest = Sha256::digest(format!("{FINGERPRINT_SALT}{sampled}{version}").as_bytes());
    hex::encode(digest)[..3].to_string()
}

/// Deterministic RFC 4122 v4 id from a seed (`ClaudeDeterministicPromptID`).
fn deterministic_prompt_id(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).hyphenated().to_string()
}

pub(crate) struct BillingTags<'a> {
    pub version: &'a str,
    pub entrypoint: &'a str,
    pub cch: bool,
    pub subagent: bool,
    pub prompt_id: Option<String>,
    pub human_turn: bool,
}

/// `x-anthropic-billing-header: cc_version=<v>.<fp>; cc_entrypoint=<e>; cch=00000; …`
pub(crate) fn billing_header(message_text: &str, tags: &BillingTags<'_>) -> String {
    let mut out = format!(
        "{BILLING_HEADER_PREFIX} cc_version={}.{}; cc_entrypoint={};",
        tags.version,
        build_fingerprint(message_text, tags.version),
        if tags.entrypoint.is_empty() {
            "cli"
        } else {
            tags.entrypoint
        }
    );
    if tags.cch {
        out.push(' ');
        out.push_str(CCH_PLACEHOLDER);
    }
    if tags.subagent {
        out.push_str(" cc_is_subagent=true;");
    }
    if tags.cch {
        if let Some(prompt_id) = &tags.prompt_id {
            out.push_str(&format!(" cc_prompt_id={prompt_id};"));
        }
        if tags.human_turn {
            out.push_str(" cc_turn_origin=human;");
        }
    }
    out
}

/// Billing header for a body that has none yet (native fallback path).
pub(crate) fn fallback_billing_header(body: &Value, entrypoint: &str, subagent: bool) -> String {
    let text = billing_message_text(body);
    let probe = profile::is_probe_or_helper(body);
    billing_header(
        &text,
        &BillingTags {
            version: profile::CLI_VERSION,
            entrypoint,
            cch: true,
            subagent,
            prompt_id: (!probe).then(|| deterministic_prompt_id(&format!("cpa:prompt:{text}"))),
            human_turn: false,
        },
    )
}

// ── Caller system prompt relocation ──────────────────────────────────────────

/// Collect the caller's system text, dropping Claude Code attribution and
/// identity blocks. Non-text blocks cannot live in any system slot.
fn collect_caller_system(system: Option<&Value>) -> Result<Vec<String>, String> {
    let keep =
        |text: &str| !text.trim().is_empty() && !is_billing_text(text) && !is_identity_text(text);
    match system {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(keep(text).then(|| text.clone()).into_iter().collect()),
        Some(Value::Array(blocks)) => {
            let mut out = Vec::new();
            for (idx, block) in blocks.iter().enumerate() {
                let block_type = str_of(block, "type");
                if block_type != "text" {
                    return Err(format!(
                        "system.{idx}.type: Input should be 'text'. System instructions support \
                         text only, but this block has type {:?}. Move non-text content into a \
                         user message.",
                        if block_type.is_empty() {
                            "unknown"
                        } else {
                            block_type
                        }
                    ));
                }
                let text = str_of(block, "text");
                if keep(text) {
                    out.push(text.to_string());
                }
            }
            Ok(out)
        }
        Some(_) => Err("system must be a string or an array of text blocks".to_string()),
    }
}

/// Anthropic binds encrypted advisor results to the message layout, so a
/// mid-conversation splice would 400; such requests keep system top-level.
fn history_has_advisor(body: &Value) -> bool {
    let is_advisor_block = |block: &Value| match str_of(block, "type") {
        "advisor_tool_result" | "advisor_redacted_result" => true,
        "server_tool_use" => str_of(block, "name") == "advisor",
        "tool_result" => match block.get("content") {
            Some(Value::Array(items)) => items
                .iter()
                .any(|item| str_of(item, "type") == "advisor_redacted_result"),
            Some(inner @ Value::Object(_)) => str_of(inner, "type") == "advisor_redacted_result",
            _ => false,
        },
        _ => false,
    };
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| match message.get("content") {
                Some(Value::Array(blocks)) => blocks.iter().any(is_advisor_block),
                Some(content @ Value::Object(_)) => matches!(
                    str_of(content, "type"),
                    "advisor_tool_result" | "advisor_redacted_result"
                ),
                _ => false,
            })
        })
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| str_of(part, "type") == "text")
            .map(|part| str_of(part, "text"))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Insert caller system blocks as `role: system` turns right after the
/// conversation's leading user run (idempotent across retries/replays).
fn insert_mid_conversation_system(body: &mut Value, blocks: &[String]) {
    let Some(first_user) = first_user_index(body) else {
        return;
    };
    let Some(messages) = messages_mut(body) else {
        return;
    };
    let mut insert_at = first_user + 1;
    while insert_at < messages.len() {
        let message = &messages[insert_at];
        let directive = str_of(message, "role") == "system"
            && message.get("output_config").is_some()
            && message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
        if str_of(message, "role") != "user" && !directive {
            break;
        }
        insert_at += 1;
    }
    let already = messages.len() - insert_at >= blocks.len()
        && blocks.iter().enumerate().all(|(i, text)| {
            let message = &messages[insert_at + i];
            str_of(message, "role") == "system" && content_text(message.get("content")) == *text
        });
    if already {
        return;
    }
    for (offset, text) in blocks.iter().enumerate() {
        messages.insert(
            insert_at + offset,
            json!({"role": "system", "content": [text_block(text, true)]}),
        );
    }
}

/// Legacy models: caller system text becomes `<system-reminder>` blocks in the
/// first user turn (after any leading `tool_result` blocks).
fn prepend_system_reminders(body: &mut Value, blocks: &[String]) {
    let Some(first_user) = first_user_index(body) else {
        return;
    };
    let content = &mut body["messages"][first_user]["content"];
    match content {
        Value::Array(parts) => {
            let mut existing: Vec<String> = parts
                .iter()
                .filter(|p| str_of(p, "type") == "text")
                .map(|p| str_of(p, "text").to_string())
                .collect();
            let mut reminders = Vec::new();
            for text in blocks {
                let reminder = caller_system_reminder(text);
                if let Some(pos) = existing.iter().position(|e| *e == reminder) {
                    existing.remove(pos);
                    continue;
                }
                reminders.push(text_block(&reminder, false));
            }
            let insert_at = parts
                .iter()
                .take_while(|p| str_of(p, "type") == "tool_result")
                .count();
            for (offset, block) in reminders.into_iter().enumerate() {
                parts.insert(insert_at + offset, block);
            }
        }
        Value::String(text) => {
            let original = std::mem::take(text);
            let mut parts: Vec<Value> = blocks
                .iter()
                .map(|t| text_block(&caller_system_reminder(t), false))
                .collect();
            parts.push(text_block(&original, false));
            *content = Value::Array(parts);
        }
        _ => {}
    }
}

/// Put the `currentDate` reminder first in the first user turn and mark the
/// first real user text as a cache breakpoint (`injectClaudeCodeCurrentDate`).
fn inject_current_date(body: &mut Value, date: &str) {
    let Some(first_user) = first_user_index(body) else {
        return;
    };
    let date_block = text_block(&date_reminder(date), false);
    let content = &mut body["messages"][first_user]["content"];
    match content {
        Value::String(text) => {
            let mut user_text = std::mem::take(text);
            if is_date_reminder(&user_text)
                && let Some(end) = user_text.find("</system-reminder>")
            {
                user_text = user_text[end + "</system-reminder>".len()..]
                    .trim_start_matches('\n')
                    .to_string();
            }
            let cache = !user_text.trim().is_empty();
            *content = Value::Array(vec![date_block, text_block(&user_text, cache)]);
        }
        Value::Array(parts) => {
            parts.retain(|p| !(str_of(p, "type") == "text" && is_date_reminder(str_of(p, "text"))));
            if let Some(target) = parts.iter_mut().find(|p| {
                let text = str_of(p, "text");
                str_of(p, "type") == "text" && !is_context_reminder(text) && !text.trim().is_empty()
            }) {
                target["cache_control"] = ephemeral();
            }
            let insert_at = parts
                .iter()
                .take_while(|p| str_of(p, "type") == "tool_result")
                .count();
            parts.insert(insert_at, date_block);
        }
        _ => {}
    }
}

fn is_haiku_55(model: &str) -> bool {
    ["claude-haiku-5-5", "claude-haiku-5.5"]
        .iter()
        .any(|family| {
            model == *family
                || model.starts_with(&format!("{family}-"))
                || model.starts_with(&format!("{family}["))
        })
}

fn is_fable_51_model(model: &str) -> bool {
    ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"]
        .iter()
        .any(|target| {
            model.match_indices(target).any(|(idx, _)| {
                model
                    .as_bytes()
                    .get(idx + target.len())
                    .is_none_or(|b| !b.is_ascii_digit())
            })
        })
}

fn is_fable_51_only(model: &str) -> bool {
    ["fable-5-1", "fable-5.1"].iter().any(|target| {
        model.match_indices(target).any(|(idx, _)| {
            model
                .as_bytes()
                .get(idx + target.len())
                .is_none_or(|b| !b.is_ascii_digit())
        })
    })
}

fn is_opus_55(model: &str) -> bool {
    model == "claude-opus-5-5" || model.starts_with("claude-opus-5-5[")
}

fn is_sonnet_55(model: &str) -> bool {
    model == "claude-sonnet-5-5"
        || model.starts_with("claude-sonnet-5-5-")
        || model.starts_with("claude-sonnet-5-5[")
}

fn is_sonnet_5(model: &str) -> bool {
    !is_sonnet_55(model)
        && (model == "claude-sonnet-5"
            || model.starts_with("claude-sonnet-5-")
            || model.starts_with("claude-sonnet-5["))
}

fn uses_progress_display(model: &str) -> bool {
    is_opus_55(model) || is_fable_51_model(model) || is_sonnet_5(model) || is_sonnet_55(model)
}

// ── Body normalizations shared by every caller ───────────────────────────────

fn thinking_type(body: &Value) -> String {
    body.pointer("/thinking/type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn tool_choice_forces_tool_use(body: &Value) -> bool {
    matches!(
        body.pointer("/tool_choice/type").and_then(Value::as_str),
        Some("any" | "tool")
    )
}

/// Anthropic rejects thinking with a forced `tool_choice`.
pub(crate) fn disable_thinking_if_tool_choice_forced(body: &mut Value) {
    if !tool_choice_forces_tool_use(body) {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    obj.remove("thinking");
    if let Some(Value::Object(config)) = obj.get_mut("output_config") {
        config.remove("effort");
        if config.is_empty() {
            obj.remove("output_config");
        }
    }
}

/// Keep sampling knobs Anthropic accepts (`normalizeClaudeSamplingForUpstream`).
pub(crate) fn normalize_sampling(body: &mut Value, native: bool) {
    let thinking_active = matches!(
        thinking_type(body).as_str(),
        "enabled" | "adaptive" | "auto"
    );
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if !native {
        obj.remove("temperature");
        obj.remove("top_p");
        if thinking_active {
            obj.remove("top_k");
        }
        return;
    }
    if thinking_active {
        if obj
            .get("temperature")
            .and_then(Value::as_f64)
            .is_some_and(|t| t != 1.0)
        {
            obj.remove("temperature");
        }
        if obj
            .get("top_p")
            .and_then(Value::as_f64)
            .is_some_and(|p| p < 0.95)
        {
            obj.remove("top_p");
        }
        obj.remove("top_k");
    } else if obj.contains_key("temperature") && obj.contains_key("top_p") {
        obj.remove("top_p");
    }
}

/// Remove `allowed_domains` / `blocked_domains` empty lists from web_search
/// tools; Anthropic rejects an empty list as ambiguous.
pub(crate) fn sanitize_web_search_domains(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if !str_of(tool, "type").starts_with("web_search_") {
            continue;
        }
        if let Some(obj) = tool.as_object_mut() {
            for field in ["allowed_domains", "blocked_domains"] {
                if obj
                    .get(field)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                {
                    obj.remove(field);
                }
            }
        }
    }
}

/// Lift body `betas` (array or comma string) out for the header.
pub(crate) fn take_body_betas(body: &mut Value) -> Vec<String> {
    let Some(raw) = body.as_object_mut().and_then(|obj| obj.remove("betas")) else {
        return Vec::new();
    };
    match raw {
        Value::String(text) => profile::split_betas(&text),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(profile::split_betas)
            .collect(),
        _ => Vec::new(),
    }
}

/// Preserved-thinking models bind signed thinking to the conversation prefix;
/// a relayed or reshaped history then fails validation unless bound blocks are
/// dropped on mismatch (`applyClaudePreservedThinkingBinding`).
pub(crate) fn apply_preserved_thinking_binding(body: &mut Value) {
    let model = canonical_model(body);
    if !(is_opus_55(&model)
        || is_sonnet_55(&model)
        || is_haiku_55(&model)
        || is_fable_51_only(&model))
        || tool_choice_forces_tool_use(body)
    {
        return;
    }
    let thinking = body.get("thinking");
    if let Some(thinking) = thinking {
        let accepts = match str_of(thinking, "type")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "adaptive" => true,
            "enabled" => !is_haiku_55(&model) && !is_sonnet_55(&model),
            _ => false,
        };
        if !accepts
            || thinking
                .pointer("/block_binding/prefix_mismatch_behavior")
                .is_some()
        {
            return;
        }
    }
    let signed = body
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| match str_of(b, "type") {
                            "thinking" => !str_of(b, "signature").trim().is_empty(),
                            "redacted_thinking" => !str_of(b, "data").trim().is_empty(),
                            _ => false,
                        })
                    })
            })
        });
    if !signed {
        return;
    }
    let thinking = body.as_object_mut().map(|obj| {
        obj.entry("thinking")
            .or_insert_with(|| json!({"type": "adaptive"}))
    });
    if let Some(Value::Object(thinking)) = thinking {
        let binding = thinking
            .entry("block_binding")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(binding) = binding {
            binding.insert("prefix_mismatch_behavior".into(), json!("drop_block"));
        }
    }
}

// ── cache_control layout ─────────────────────────────────────────────────────

/// Visit every block that can carry `cache_control`, in Anthropic's
/// evaluation order (tools → system → messages).
fn for_each_cache_host(body: &mut Value, mut visit: impl FnMut(&mut Value)) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        tools.iter_mut().for_each(&mut visit);
    }
    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
        system.iter_mut().for_each(&mut visit);
    }
    if let Some(messages) = messages_mut(body) {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                content.iter_mut().for_each(&mut visit);
            }
        }
    }
}

fn count_cache_controls(body: &Value) -> usize {
    let count = |items: Option<&Value>| {
        items.and_then(Value::as_array).map_or(0, |items| {
            items
                .iter()
                .filter(|item| valid_cache_control(item.get("cache_control")))
                .count()
        })
    };
    count(body.get("tools"))
        + count(body.get("system"))
        + body
            .get("messages")
            .and_then(Value::as_array)
            .map_or(0, |messages| {
                messages.iter().map(|m| count(m.get("content"))).sum()
            })
}

fn system_block_hosts_cache(block: &Value) -> bool {
    let block_type = str_of(block, "type");
    block.is_object()
        && (block_type.is_empty() || block_type == "text")
        && !str_of(block, "text").trim().is_empty()
}

fn content_block_hosts_cache(block: &Value) -> bool {
    match str_of(block, "type") {
        "" | "thinking" | "redacted_thinking" => false,
        "text" => !str_of(block, "text").trim().is_empty(),
        _ => block.is_object(),
    }
}

fn last_cacheable_content_index(message: &Value) -> Option<usize> {
    let blocks = message.get("content")?.as_array()?;
    let assistant = str_of(message, "role") == "assistant";
    for (idx, block) in blocks.iter().enumerate().rev() {
        if str_of(block, "type") == "text" && str_of(block, "text").trim().is_empty() {
            continue;
        }
        if assistant && matches!(str_of(block, "type"), "thinking" | "redacted_thinking") {
            return None;
        }
        if content_block_hosts_cache(block) {
            return Some(idx);
        }
    }
    None
}

fn eligible_for_rolling_cache(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        _ => last_cacheable_content_index(message).is_some(),
    }
}

/// Default breakpoints for cloaked traffic (`ensureCacheControl`): the last
/// system block, plus the native rolling breakpoint on the latest eligible turn.
fn ensure_cache_control(body: &mut Value) {
    let has_cacheable_system = match body.get("system") {
        Some(Value::Array(blocks)) => blocks.iter().any(system_block_hosts_cache),
        Some(Value::String(text)) => !text.trim().is_empty(),
        _ => false,
    };
    if !has_cacheable_system
        && let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
        && !tools
            .iter()
            .any(|t| valid_cache_control(t.get("cache_control")))
        && let Some(last) = tools
            .iter_mut()
            .rev()
            .find(|t| t.get("defer_loading").and_then(Value::as_bool) != Some(true))
    {
        last["cache_control"] = ephemeral();
    }
    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut)
        && !system
            .iter()
            .any(|b| valid_cache_control(b.get("cache_control")))
        && let Some(last) = system
            .iter_mut()
            .rev()
            .find(|b| system_block_hosts_cache(b))
    {
        last["cache_control"] = ephemeral();
    }

    let Some(messages) = messages_mut(body) else {
        return;
    };
    let last_eligible = messages.iter().rposition(|m| {
        matches!(str_of(m, "role"), "user" | "assistant") && eligible_for_rolling_cache(m)
    });
    let Some(target) = last_eligible else {
        return;
    };
    if let Some(final_message) = messages.last_mut()
        && str_of(final_message, "role") == "system"
        && let Some(text) = final_message
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
    {
        final_message["content"] = json!([text_block(&text, true)]);
        return;
    }
    let message = &mut messages[target];
    enum Plan {
        Block(usize),
        Wrap(String),
        Nothing,
    }
    let plan = match message.get("content") {
        Some(Value::Array(blocks)) => {
            if blocks
                .iter()
                .any(|b| valid_cache_control(b.get("cache_control")))
            {
                Plan::Nothing
            } else {
                last_cacheable_content_index(message).map_or(Plan::Nothing, Plan::Block)
            }
        }
        Some(Value::String(text)) if !text.trim().is_empty() => Plan::Wrap(text.clone()),
        _ => Plan::Nothing,
    };
    match plan {
        Plan::Block(idx) => message["content"][idx]["cache_control"] = ephemeral(),
        Plan::Wrap(text) => message["content"] = json!([text_block(&text, true)]),
        Plan::Nothing => {}
    }
}

/// Drop excess breakpoints (`enforceCacheControlLimit`): earlier system and
/// tool markers first (keeping the last of each), then message markers
/// earliest-first, then the remaining system and tool markers.
pub(crate) fn enforce_cache_control_limit(body: &mut Value) {
    let mut max = MAX_CACHE_BREAKPOINTS;
    if body.get("thread").is_some_and(|t| !t.is_null()) {
        max -= 1;
    }
    let total = count_cache_controls(body);
    if total <= max {
        return;
    }
    let mut excess = total - max;
    let strip_section = |items: Option<&mut Vec<Value>>, keep_last: bool, excess: &mut usize| {
        let Some(items) = items else {
            return;
        };
        let last = items
            .iter()
            .rposition(|item| valid_cache_control(item.get("cache_control")));
        for (idx, item) in items.iter_mut().enumerate() {
            if *excess == 0 {
                return;
            }
            if keep_last && Some(idx) == last {
                continue;
            }
            if valid_cache_control(item.get("cache_control"))
                && let Some(obj) = item.as_object_mut()
            {
                obj.remove("cache_control");
                *excess -= 1;
            }
        }
    };
    strip_section(
        body.get_mut("system").and_then(Value::as_array_mut),
        true,
        &mut excess,
    );
    strip_section(
        body.get_mut("tools").and_then(Value::as_array_mut),
        true,
        &mut excess,
    );
    if let Some(messages) = messages_mut(body) {
        for message in messages {
            strip_section(
                message.get_mut("content").and_then(Value::as_array_mut),
                false,
                &mut excess,
            );
        }
    }
    strip_section(
        body.get_mut("system").and_then(Value::as_array_mut),
        false,
        &mut excess,
    );
    strip_section(
        body.get_mut("tools").and_then(Value::as_array_mut),
        false,
        &mut excess,
    );
}

/// Upgrade every marker without a ttl to the 1h pool the OAuth CLI uses.
fn upgrade_cache_ttl(body: &mut Value) {
    for_each_cache_host(body, |block| {
        if let Some(cc) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
            && cc.get("type").is_some_and(Value::is_string)
            && !cc.contains_key("ttl")
        {
            cc.insert("ttl".into(), json!("1h"));
        }
    });
}

fn strip_cache_ttl(body: &mut Value) {
    for_each_cache_host(body, |block| {
        if let Some(cc) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        {
            cc.remove("ttl");
        }
    });
}

/// A 1h marker may not follow a 5m one in evaluation order; downgrade those.
pub(crate) fn normalize_cache_ttl_order(body: &mut Value) {
    let mut seen_5m = false;
    for_each_cache_host(body, |block| {
        if !valid_cache_control(block.get("cache_control")) {
            return;
        }
        let Some(cc) = block
            .get_mut("cache_control")
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        if cc.get("ttl").and_then(Value::as_str) != Some("1h") {
            seen_5m = true;
        } else if seen_5m {
            cc.remove("ttl");
        }
    });
}

// ── Cloaking entry point ─────────────────────────────────────────────────────

/// Facts about the request the cloak needs but cannot read from the body.
pub(crate) struct CloakContext<'a> {
    /// Local calendar date `YYYY-MM-DD` for the `currentDate` reminder.
    pub date: &'a str,
    /// Subagent markers seen on the incoming request.
    pub subagent: bool,
    /// Upstream is first-party `api.anthropic.com`.
    pub first_party: bool,
}

/// Rewrite a non-Claude-Code request into the Claude Code CLI shape.
pub(crate) fn cloak(body: &mut Value, ctx: &CloakContext<'_>) -> Result<(), String> {
    let caller_system = collect_caller_system(body.get("system"))?;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("prompt_cache_options");
    }

    let probe = profile::is_probe_or_helper(body);
    let model = canonical_model(body);
    let message_text = billing_message_text(body);
    let billing = billing_header(
        &message_text,
        &BillingTags {
            version: profile::CLI_VERSION,
            entrypoint: "cli",
            cch: true,
            subagent: ctx.subagent,
            prompt_id: (!probe)
                .then(|| deterministic_prompt_id(&format!("cpa:prompt:{message_text}"))),
            human_turn: !probe && !ctx.subagent,
        },
    );
    let mut system = vec![
        text_block(&billing, false),
        text_block(CLAUDE_CODE_IDENTITY, true),
    ];
    if is_fable_51_model(&model) && !probe {
        system.push(text_block(FABLE_REPORTING_OUTCOMES, false));
    }

    if !caller_system.is_empty() {
        if history_has_advisor(body) {
            system.extend(caller_system.iter().map(|text| text_block(text, true)));
        } else if profile::uses_legacy_system_reminder(body) {
            prepend_system_reminders(body, &caller_system);
        } else {
            insert_mid_conversation_system(body, &caller_system);
        }
    }
    body["system"] = Value::Array(system);
    inject_current_date(body, ctx.date);

    if !probe && body.get("fallbacks").is_none() {
        if is_opus_55(&model) {
            body["fallbacks"] = json!([{"model": "claude-opus-4-8"}]);
        } else if is_fable_51_model(&model) {
            body["fallbacks"] = json!([{"model": "claude-opus-5"}]);
        }
    }
    if !probe
        && body.pointer("/thinking/display").is_none()
        && uses_progress_display(&model)
        && matches!(thinking_type(body).as_str(), "adaptive" | "enabled")
    {
        body["thinking"]["display"] = json!("updates");
    }

    disable_thinking_if_tool_choice_forced(body);
    // Claude Code sends this on every first-party request; the beta for it is
    // always claimed. Anthropic rejects it unless thinking is on.
    if ctx.first_party
        && body.get("context_management").is_none()
        && matches!(thinking_type(body).as_str(), "enabled" | "adaptive")
    {
        body["context_management"] =
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]});
    }
    normalize_sampling(body, false);

    ensure_cache_control(body);
    enforce_cache_control_limit(body);
    if !probe && !ctx.subagent {
        upgrade_cache_ttl(body);
    } else {
        strip_cache_ttl(body);
    }
    normalize_cache_ttl_order(body);
    sanitize_web_search_domains(body);
    apply_preserved_thinking_binding(body);
    Ok(())
}

/// count_tokens counterpart of the cloak's system handling: the caller's
/// system text is placed where the Messages path places it, and no Claude
/// Code blocks are installed (native count_tokens carries no system).
pub(crate) fn relocate_caller_system_for_count(body: &mut Value, system: Option<&Value>) {
    let Ok(blocks) = collect_caller_system(system) else {
        return;
    };
    if blocks.is_empty() {
        return;
    }
    if history_has_advisor(body) {
        body["system"] = Value::Array(blocks.iter().map(|t| text_block(t, true)).collect());
    } else if profile::uses_legacy_system_reminder(body) {
        prepend_system_reminders(body, &blocks);
    } else {
        insert_mid_conversation_system(body, &blocks);
    }
}

/// Native Claude Code behind a non-first-party base URL omits `cch`; restore
/// it, or add the fallback billing block when the body has a system prompt
/// but no billing header (helpers with no system are left as sent).
pub(crate) fn ensure_native_billing(body: &mut Value, entrypoint: &str, subagent: bool) {
    let first_text = body
        .pointer("/system/0/text")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(text) = first_text.filter(|t| is_billing_text(t)) {
        if let Some(updated) = super::cch::ensure_placeholder(&text) {
            body["system"][0]["text"] = json!(updated);
        }
        return;
    }
    if body.get("system").is_none() {
        return;
    }
    let billing = text_block(&fallback_billing_header(body, entrypoint, subagent), false);
    let system = match body.get_mut("system").map(Value::take) {
        Some(Value::String(text)) => vec![billing, text_block(&text, false)],
        Some(Value::Array(mut blocks)) => {
            blocks.insert(0, billing);
            blocks
        }
        _ => vec![billing],
    };
    body["system"] = Value::Array(system);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CloakContext<'static> {
        CloakContext {
            date: "2026-10-28",
            subagent: false,
            first_party: true,
        }
    }

    #[test]
    fn fingerprint_samples_utf16_positions() {
        // Deterministic and version-dependent.
        let a = build_fingerprint("Hello, this is a test prompt", "2.1.280");
        assert_eq!(a.len(), 3);
        assert_eq!(
            a,
            build_fingerprint("Hello, this is a test prompt", "2.1.280")
        );
        assert_ne!(
            a,
            build_fingerprint("Hello, this is a test prompt", "2.1.281")
        );
        // Short text pads missing positions with '0'.
        let expected = hex::encode(Sha256::digest(
            format!("{FINGERPRINT_SALT}0002.1.280").as_bytes(),
        ));
        assert_eq!(build_fingerprint("hi", "2.1.280"), expected[..3]);
        // CLIProxyAPI vectors (claude_cloaked_cache_repro_test.go): UTF-16
        // indexing, and a sampled lone surrogate becoming U+FFFD.
        assert_eq!(
            build_fingerprint("Hello there, please help me", "2.1.280"),
            "0e5"
        );
        assert_eq!(
            build_fingerprint("😀abcdefghijklmnopqrstuvwxyz", "2.1.280"),
            "dac"
        );
        assert_eq!(build_fingerprint("abc😀abcdefghijklmnop", "2.1.280"), "695");
    }

    #[test]
    fn cloak_builds_claude_code_system_and_relocates_caller_prompt() {
        let mut body = json!({
            "model": "claude-opus-5",
            "system": [{"type": "text", "text": "You are a helpful bot."}],
            "temperature": 0.2,
            "top_p": 0.9,
            "thinking": {"type": "adaptive"},
            "messages": [
                {"role": "user", "content": "hello there, please help"},
                {"role": "assistant", "content": "sure"},
                {"role": "user", "content": "next"}
            ]
        });
        cloak(&mut body, &ctx()).unwrap();

        let billing = body["system"][0]["text"].as_str().unwrap();
        assert!(
            billing.starts_with("x-anthropic-billing-header: cc_version=2.1.280."),
            "{billing}"
        );
        assert!(
            billing.contains("; cc_entrypoint=cli; cch=00000;"),
            "{billing}"
        );
        assert!(billing.contains(" cc_prompt_id="));
        assert!(billing.ends_with(" cc_turn_origin=human;"));
        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(body["system"][1]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);

        // Caller system prompt is a mid-conversation system turn after the
        // first user run.
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[1]["content"][0]["text"], "You are a helpful bot.");
        // currentDate reminder leads the first user turn.
        assert!(
            messages[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Today's date is 2026-10-28.")
        );
        assert_eq!(
            messages[0]["content"][1]["text"],
            "hello there, please help"
        );

        assert!(body.get("temperature").is_none() && body.get("top_p").is_none());
        assert_eq!(
            body["context_management"]["edits"][0]["type"],
            "clear_thinking_20251015"
        );
        assert!(count_cache_controls(&body) <= MAX_CACHE_BREAKPOINTS);
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn legacy_models_get_system_reminders_and_cloak_is_idempotent() {
        let mut body = json!({
            "model": "claude-sonnet-4-5-20250929",
            "system": "Rules: be terse.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "do it"}]}]
        });
        cloak(&mut body, &ctx()).unwrap();
        let once = body.clone();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("# currentDate")
        );
        assert_eq!(
            content[1]["text"],
            "<system-reminder>\nRules: be terse.\n</system-reminder>"
        );
        assert_eq!(content[2]["text"], "do it");
        assert!(
            body.get("context_management").is_none(),
            "no thinking → no clear_thinking"
        );

        // A replay of the already-cloaked body must not stack blocks.
        cloak(&mut body, &ctx()).unwrap();
        assert_eq!(body["messages"], once["messages"]);
        assert_eq!(body["system"], once["system"]);
    }

    #[test]
    fn forced_tool_choice_disables_thinking_and_skips_context_management() {
        let mut body = json!({
            "model": "claude-opus-4-7",
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "output_config": {"effort": "high"},
            "tool_choice": {"type": "any"},
            "messages": [{"role": "user", "content": "x"}]
        });
        cloak(&mut body, &ctx()).unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("context_management").is_none());
    }

    #[test]
    fn non_text_system_blocks_are_rejected() {
        let mut body = json!({"system": [{"type": "image", "source": {}}], "messages": []});
        assert!(
            cloak(&mut body, &ctx())
                .unwrap_err()
                .contains("system.0.type")
        );
    }

    #[test]
    fn cache_limit_keeps_last_system_marker() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}
            ],
            "tools": [{"name": "t", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]},
                {"role": "user", "content": [{"type": "text", "text": "y", "cache_control": {"type": "ephemeral"}}]}
            ]
        });
        enforce_cache_control_limit(&mut body);
        assert_eq!(count_cache_controls(&body), 4);
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["system"][1].get("cache_control").is_some());
    }

    #[test]
    fn ttl_order_downgrades_1h_after_5m() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]
        });
        normalize_cache_ttl_order(&mut body);
        assert!(body["system"][1]["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn native_billing_gets_cch_placeholder_or_fallback_block() {
        let mut native = json!({"system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli;"}]});
        ensure_native_billing(&mut native, "cli", false);
        assert_eq!(
            native["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli; cch=00000;"
        );

        let mut no_billing =
            json!({"system": "custom", "messages": [{"role": "user", "content": "q"}]});
        ensure_native_billing(&mut no_billing, "sdk-cli", false);
        let first = no_billing["system"][0]["text"].as_str().unwrap();
        assert!(
            first.contains("cc_entrypoint=sdk-cli; cch=00000;"),
            "{first}"
        );
        assert_eq!(no_billing["system"][1]["text"], "custom");

        let mut helper = json!({"messages": [{"role": "user", "content": "q"}]});
        ensure_native_billing(&mut helper, "cli", false);
        assert!(helper.get("system").is_none());
    }

    #[test]
    fn native_sampling_keeps_valid_knobs() {
        let mut body =
            json!({"temperature": 1, "top_p": 0.99, "top_k": 5, "thinking": {"type": "enabled"}});
        normalize_sampling(&mut body, true);
        assert_eq!(body["temperature"], 1);
        assert_eq!(body["top_p"], 0.99);
        assert!(body.get("top_k").is_none());
        let mut plain = json!({"temperature": 0.3, "top_p": 0.9});
        normalize_sampling(&mut plain, true);
        assert!(plain.get("top_p").is_none());
        assert_eq!(plain["temperature"], 0.3);
    }
}
