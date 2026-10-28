//! Claude Code-style MCP aliases for client tool names.
//!
//! Anthropic fingerprints tool names on OAuth (subscription) traffic: a client
//! whose tools are not Claude Code built-ins or `mcp__<server>__<tool>` MCP
//! extensions is treated as a third-party harness (CLIProxyAPI A/B result,
//! commit e8d1b79c). Cloaked requests therefore rename every client-declared
//! tool to an opaque but deterministic MCP alias and restore the original names
//! in the response. Ported from CLIProxyAPI `helps/claude_mcp_alias.go` and
//! `remapOAuthToolNamesWithBatchedEdits` / `reverseRemapOAuthToolNames*`.
//!
//! Aliases are `mcp__<word>_<word>__<word>_<semantic>`: the two-word virtual
//! server and the tool word are BIP-39 words picked by HMAC-SHA256 over a
//! caller-scoped secret, and the semantic suffix keeps a sanitized copy of the
//! original name so the model can still tell tools apart.

use std::collections::{HashMap, HashSet};

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

const BIP39_ENGLISH: &str = include_str!("../../../../assets/bip39_english.txt");
const ALIAS_DOMAIN: &[u8] = b"cpa-claude-mcp-alias-v2\x00";
const MAX_TOOL_NAME_LEN: usize = 64;

/// Names Anthropic reserves for its own tools even when not declared.
const BUILTIN_TOOL_NAMES: &[&str] = &["web_search", "code_execution", "text_editor", "computer"];

/// Type prefixes of Anthropic-operated (server) tools, whose names must stay.
const SERVER_TOOL_TYPE_PREFIXES: &[&str] = &[
    "advisor_",
    "agent_toolset_",
    "bash_",
    "code_execution_",
    "computer_",
    "memory_",
    "text_editor_",
    "tool_search_tool_",
    "web_fetch_",
    "web_search_",
];

fn words() -> &'static [&'static str] {
    static WORDS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    WORDS.get_or_init(|| {
        BIP39_ENGLISH
            .lines()
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .collect()
    })
}

pub(crate) fn is_server_tool_type(tool_type: &str) -> bool {
    let tool_type = tool_type.trim().to_ascii_lowercase();
    SERVER_TOOL_TYPE_PREFIXES
        .iter()
        .any(|prefix| tool_type.starts_with(prefix))
}

fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// Whether `name` already follows Claude Code's `mcp__<server>__<tool>` form.
pub(crate) fn is_mcp_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN || !name.chars().all(is_name_char) {
        return false;
    }
    let Some(rest) = name.strip_prefix("mcp__") else {
        return false;
    };
    matches!(rest.find("__"), Some(sep) if sep > 0 && sep + 2 < rest.len())
}

fn digest(secret: &str, purpose: &str, original: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(ALIAS_DOMAIN);
    mac.update(purpose.as_bytes());
    mac.update(&[0]);
    mac.update(original.as_bytes());
    mac.finalize().into_bytes().into()
}

fn word_at(digest: &[u8; 32], offset: usize, attempt: usize) -> &'static str {
    let words = words();
    let base = usize::from(u16::from_be_bytes([digest[offset], digest[offset + 1]]));
    words[(base + attempt) % words.len()]
}

fn server_component(secret: &str) -> String {
    let d = digest(secret, "server", "");
    format!("{}_{}", word_at(&d, 0, 0), word_at(&d, 2, 0))
}

fn semantic_suffix(original: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(original.len().min(max_len));
    let mut pending_separator = false;
    for ch in original.chars() {
        if !is_name_char(ch) {
            pending_separator = !out.is_empty();
            continue;
        }
        if pending_separator && out.len() + 1 < max_len {
            out.push('_');
        }
        pending_separator = false;
        if out.len() >= max_len {
            break;
        }
        out.push(ch);
    }
    let trimmed = out.trim_matches(|c| c == '_' || c == '-');
    if trimmed.is_empty() {
        "tool".to_string()
    } else {
        trimmed.to_string()
    }
}

fn alias_for(server: &str, tool_word: &str, original: &str) -> String {
    let prefix = format!("mcp__{server}__{tool_word}_");
    let max = MAX_TOOL_NAME_LEN.saturating_sub(prefix.len()).max(1);
    format!("{prefix}{}", semantic_suffix(original, max))
}

/// Allocate the first alias for `original` not already in `reserved`.
fn allocate_alias(secret: &str, original: &str, reserved: &HashSet<String>) -> Option<String> {
    let total = words().len();
    let server = server_component(secret);
    let d = digest(secret, "tool", original);
    let base = usize::from(u16::from_be_bytes([d[0], d[1]])) % total;
    (0..total)
        .map(|attempt| alias_for(&server, words()[(base + attempt) % total], original))
        .find(|alias| !reserved.contains(alias))
}

/// Request-local `alias → original` table used to restore response names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolAliasMap {
    reverse: HashMap<String, String>,
}

impl ToolAliasMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    pub(crate) fn original(&self, name: &str) -> Option<&str> {
        self.reverse.get(name).map(String::as_str)
    }
}

/// Rename every client-declared tool to its MCP alias in `tools[]`,
/// `tool_choice` and message history. Server tools and names that are already
/// MCP-shaped are left alone. Returns the reverse table (empty when nothing
/// was renamed).
pub(crate) fn alias_request_tools(body: &mut Value, secret: &str) -> ToolAliasMap {
    let mut forward: HashMap<String, String> = HashMap::new();
    let mut protected: HashSet<String> = HashSet::new();
    let mut reserved: HashSet<String> = BUILTIN_TOOL_NAMES.iter().map(|s| s.to_string()).collect();

    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return ToolAliasMap::default();
    };
    for tool in tools.iter() {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        reserved.insert(name.to_string());
        if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_server_tool_type)
        {
            protected.insert(name.to_string());
        }
    }
    for tool in tools.iter() {
        let server_tool = tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_server_tool_type);
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        if server_tool || name.is_empty() || is_mcp_tool_name(name) || forward.contains_key(name) {
            continue;
        }
        match allocate_alias(secret, name, &reserved) {
            Some(alias) => {
                reserved.insert(alias.clone());
                forward.insert(name.to_string(), alias);
            }
            None => tracing::warn!(
                tool = name,
                "claude-code: no free MCP alias, forwarding tool name"
            ),
        }
    }
    if forward.is_empty() {
        return ToolAliasMap::default();
    }

    let rename = |name: &str| -> Option<String> {
        if name.is_empty() || protected.contains(name) || is_mcp_tool_name(name) {
            return None;
        }
        forward.get(name).cloned()
    };
    let mut reverse: HashMap<String, String> = HashMap::new();
    let mut apply = |slot: &mut Value| {
        if let Some(name) = slot.as_str()
            && let Some(alias) = rename(name)
        {
            reverse
                .entry(alias.clone())
                .or_insert_with(|| name.to_string());
            *slot = Value::String(alias);
        }
    };

    for tool in tools.iter_mut() {
        let Some(obj) = tool.as_object_mut() else {
            continue;
        };
        if obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(is_server_tool_type)
        {
            continue;
        }
        // Typed client declarations (e.g. `"type":"custom"`) lose the type so
        // they look like a plain Claude Code MCP tool.
        if obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty())
        {
            obj.remove("type");
        }
        if let Some(slot) = obj.get_mut("name") {
            apply(slot);
        }
    }

    if let Some(choice) = body.get_mut("tool_choice")
        && choice.get("type").and_then(Value::as_str) == Some("tool")
        && let Some(slot) = choice.get_mut("name")
    {
        apply(slot);
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for part in content {
                rename_history_part(part, &mut apply);
            }
        }
    }

    ToolAliasMap { reverse }
}

fn rename_history_part(part: &mut Value, apply: &mut impl FnMut(&mut Value)) {
    let part_type = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match part_type.as_str() {
        "tool_use" => {
            if let Some(slot) = part.get_mut("name") {
                apply(slot);
            }
        }
        "tool_reference" => {
            if let Some(slot) = part.get_mut("tool_name") {
                apply(slot);
            }
        }
        "tool_result" => {
            if let Some(nested) = part.get_mut("content").and_then(Value::as_array_mut) {
                for item in nested {
                    if item.get("type").and_then(Value::as_str) == Some("tool_reference")
                        && let Some(slot) = item.get_mut("tool_name")
                    {
                        apply(slot);
                    }
                }
            }
        }
        "tool_search_tool_result" => {
            if let Some(refs) = part
                .pointer_mut("/content/tool_references")
                .and_then(Value::as_array_mut)
            {
                for item in refs {
                    if item.get("type").and_then(Value::as_str) == Some("tool_reference")
                        && let Some(slot) = item.get_mut("tool_name")
                    {
                        apply(slot);
                    }
                }
            }
        }
        "tool_addition" | "tool_removal" => {
            let tool_is_reference =
                part.pointer("/tool/type").and_then(Value::as_str) == Some("tool_reference");
            if tool_is_reference {
                if let Some(slot) = part.pointer_mut("/tool/name") {
                    apply(slot);
                }
            } else if part_type == "tool_addition"
                && !part
                    .pointer("/tool/definition/type")
                    .and_then(Value::as_str)
                    .is_some_and(is_server_tool_type)
                && let Some(slot) = part.pointer_mut("/tool/definition/name")
            {
                apply(slot);
            }
        }
        _ => {}
    }
}

// ── Response restore ─────────────────────────────────────────────────────────

/// Restore original names in one content block (`tool_use`, `tool_reference`,
/// `tool_search_tool_result`). Returns whether anything changed.
fn restore_block(block: &mut Value, map: &ToolAliasMap) -> bool {
    let mut changed = false;
    let mut restore = |slot: Option<&mut Value>| {
        if let Some(slot) = slot
            && let Some(original) = slot.as_str().and_then(|name| map.original(name))
        {
            *slot = Value::String(original.to_string());
            changed = true;
        }
    };
    match block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "tool_use" => restore(block.get_mut("name")),
        "tool_reference" => restore(block.get_mut("tool_name")),
        "tool_result" => {
            if let Some(items) = block.get_mut("content").and_then(Value::as_array_mut) {
                for item in items {
                    restore(item.get_mut("tool_name"));
                }
            }
        }
        "tool_search_tool_result" => {
            if let Some(refs) = block
                .pointer_mut("/content/tool_references")
                .and_then(Value::as_array_mut)
            {
                for item in refs {
                    restore(item.get_mut("tool_name"));
                }
            }
        }
        _ => {}
    }
    changed
}

/// Restore tool names in a non-stream Messages response body. Returns `None`
/// when nothing needed restoring so the caller can keep the original bytes.
pub(crate) fn restore_response_json(body: &[u8], map: &ToolAliasMap) -> Option<Vec<u8>> {
    if map.is_empty() || !contains(body, b"mcp__") {
        return None;
    }
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
        for block in content {
            changed |= restore_block(block, map);
        }
    }
    changed.then(|| serde_json::to_vec(&value).ok()).flatten()
}

/// Restore tool names in one complete SSE line. Only `data:` lines whose
/// event carries a `content_block` (i.e. `content_block_start`) can hold
/// names; deltas never do. Returns `None` when the line is unchanged.
pub(crate) fn restore_sse_line(line: &str, map: &ToolAliasMap) -> Option<String> {
    if map.is_empty() || !line.contains("mcp__") {
        return None;
    }
    let payload = line.strip_prefix("data:")?;
    let mut value: Value = serde_json::from_str(payload.trim_start()).ok()?;
    let block = value.get_mut("content_block")?;
    if !restore_block(block, map) {
        return None;
    }
    Some(format!("data: {}", serde_json::to_string(&value).ok()?))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DEFAULT_SECRET: &str = "cpa-claude-mcp-default-caller";

    #[test]
    fn wordlist_is_bip39_english() {
        assert_eq!(words().len(), 2048);
        assert_eq!(words()[0], "abandon");
        assert_eq!(words()[2047], "zoo");
    }

    // Values computed by CLIProxyAPI's Go implementation with its default
    // caller secret; nyro must produce byte-identical aliases.
    #[test]
    fn aliases_match_cliproxyapi() {
        let reserved = HashSet::new();
        for (original, want) in [
            ("Bash", "mcp__select_sight__term_Bash"),
            ("read_file", "mcp__select_sight__charge_read_file"),
            ("todowrite", "mcp__select_sight__unique_todowrite"),
            ("my.tool name", "mcp__select_sight__position_my_tool_name"),
        ] {
            assert_eq!(
                allocate_alias(DEFAULT_SECRET, original, &reserved).unwrap(),
                want
            );
        }
    }

    #[test]
    fn mcp_name_shape() {
        assert!(is_mcp_tool_name("mcp__github__create_issue"));
        assert!(!is_mcp_tool_name("mcp__github"));
        assert!(!is_mcp_tool_name("mcp____x"));
        assert!(!is_mcp_tool_name("Bash"));
        assert!(!is_mcp_tool_name("mcp__a__b.c"));
    }

    #[test]
    fn request_tools_are_aliased_everywhere_and_server_tools_survive() {
        let mut body = json!({
            "tools": [
                {"name": "Bash", "input_schema": {"type": "object"}},
                {"type": "custom", "name": "todowrite", "input_schema": {"type": "object"}},
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "mcp__github__create_issue", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "tool", "name": "Bash"},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "undeclared", "input": {}}
                ]}
            ]
        });
        let map = alias_request_tools(&mut body, DEFAULT_SECRET);
        assert_eq!(body["tools"][0]["name"], "mcp__select_sight__term_Bash");
        assert_eq!(
            body["tools"][1]["name"],
            "mcp__select_sight__unique_todowrite"
        );
        assert!(
            body["tools"][1].get("type").is_none(),
            "custom type is dropped"
        );
        assert_eq!(body["tools"][2]["name"], "web_search");
        assert_eq!(body["tools"][2]["type"], "web_search_20250305");
        assert_eq!(body["tools"][3]["name"], "mcp__github__create_issue");
        assert_eq!(body["tool_choice"]["name"], "mcp__select_sight__term_Bash");
        assert_eq!(
            body["messages"][1]["content"][0]["name"],
            "mcp__select_sight__term_Bash"
        );
        assert_eq!(body["messages"][1]["content"][1]["name"], "undeclared");
        assert_eq!(map.original("mcp__select_sight__term_Bash"), Some("Bash"));
        assert_eq!(
            map.original("mcp__select_sight__unique_todowrite"),
            Some("todowrite")
        );
        assert_eq!(map.original("mcp__github__create_issue"), None);
    }

    #[test]
    fn no_client_tools_means_no_map() {
        let mut body = json!({"tools": [{"type": "web_search_20250305", "name": "web_search"}]});
        assert!(alias_request_tools(&mut body, DEFAULT_SECRET).is_empty());
        let mut body = json!({"messages": []});
        assert!(alias_request_tools(&mut body, DEFAULT_SECRET).is_empty());
    }

    #[test]
    fn response_json_and_sse_names_are_restored() {
        let mut req = json!({"tools": [{"name": "Bash", "input_schema": {}}]});
        let map = alias_request_tools(&mut req, DEFAULT_SECRET);

        let body = br#"{"id":"msg_1","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"t","name":"mcp__select_sight__term_Bash","input":{"cmd":"ls"}}]}"#;
        let restored: Value =
            serde_json::from_slice(&restore_response_json(body, &map).unwrap()).unwrap();
        assert_eq!(restored["content"][1]["name"], "Bash");
        assert_eq!(restored["content"][1]["input"]["cmd"], "ls");

        let line = r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t","name":"mcp__select_sight__term_Bash","input":{}}}"#;
        let restored = restore_sse_line(line, &map).unwrap();
        assert!(restored.starts_with("data: "));
        let value: Value = serde_json::from_str(&restored["data: ".len()..]).unwrap();
        assert_eq!(value["content_block"]["name"], "Bash");

        // Deltas and unrelated lines stay byte-identical.
        let delta = r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"mcp__x\":1}"}}"#;
        assert_eq!(restore_sse_line(delta, &map), None);
        assert_eq!(restore_sse_line("event: content_block_start", &map), None);
        assert_eq!(restore_response_json(br#"{"content":[]}"#, &map), None);
    }
}
