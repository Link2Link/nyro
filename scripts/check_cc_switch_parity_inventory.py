#!/usr/bin/env python3
"""Audit cc-switch proxy tests against Nyro's wire-parity test inventory.

The source is always read from a Git commit, never from the source worktree.
A source hash is SHA-256 over the complete Rust test item beginning at its
#[test] or #[tokio::test] attribute and ending at the function's closing brace,
with line endings normalized to LF (``sha256-rust-test-item-v1``).

Target locators use ``repository/relative/file.rs::test_function_name``.  The
``--update-targets`` mode only records uniquely discoverable same-name target
candidates; it deliberately leaves PENDING mapping text in place so discovery
cannot silently claim assertion-level parity.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from typing import Any, Iterable

SOURCE_ROOT = "src-tauri/src/proxy"
HASH_ALGORITHM = "sha256-rust-test-item-v1"
ALLOWED_STATUSES = {"migrated", "mapped", "not-applicable"}

# Assertion-level mappings reviewed against both source and target test bodies.
# Keeping these here makes a fresh --initialize deterministic while target_sha256
# locks the exact reviewed target item in the TOML inventory.
REVIEWED_MAPPINGS = {
    (
        "src-tauri/src/proxy/handlers.rs",
        "body_looks_like_sse_detects_unlabeled_sse_prefixes",
    ): (
        "crates/nyro-ccswitch-compat/src/transport.rs::body_looks_like_sse_detects_unlabeled_sse_prefixes",
        "The target preserves every source positive prefix case (data, event, id, retry, comment, and BOM/whitespace) and every negative HTML, plain-text, and empty-body assertion.",
    ),
    (
        "src-tauri/src/proxy/handlers.rs",
        "codex_oauth_responses_force_streaming_even_if_client_sent_false",
    ): (
        "crates/nyro-ccswitch-compat/src/profile.rs::codex_oauth_always_forces_upstream_stream",
        "ConversionProfile::force_upstream_stream asserts that CodexOAuthResponses forces an upstream stream when client_stream is false, matching the source openai_responses plus codex_oauth branch.",
    ),
    (
        "src-tauri/src/proxy/handlers.rs",
        "chat_sse_to_response_value_rejects_truncated_stream",
    ): (
        "crates/nyro-ccswitch-compat/src/ported/handlers_compat.rs::chat_sse_requires_a_terminal_marker",
        "The target supplies a content delta with neither finish_reason nor [DONE] and asserts an error, preserving the source truncation guard and terminal-marker requirement.",
    ),
}
REQUIRED_CLASSIFICATIONS = {
    "protocol-aggregation",
    "protocol-error",
    "protocol-profile",
    "protocol-sse",
    "protocol-state",
    "protocol-support",
    "protocol-transform",
    "protocol-transport",
}

DIRECT_SOURCE_PATHS = {
    "src-tauri/src/proxy/content_encoding.rs": "protocol-transport",
    "src-tauri/src/proxy/json_canonical.rs": "protocol-support",
    "src-tauri/src/proxy/sse.rs": "protocol-sse",
    "src-tauri/src/proxy/tool_media.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/codex_chat_history.rs": "protocol-state",
    "src-tauri/src/proxy/providers/codex_responses_sse.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/gemini_schema.rs": "protocol-support",
    "src-tauri/src/proxy/providers/gemini_shadow.rs": "protocol-state",
    "src-tauri/src/proxy/providers/reasoning_bridge.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/streaming.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/streaming_codex_anthropic.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/streaming_codex_chat.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/streaming_gemini.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/streaming_responses.rs": "protocol-sse",
    "src-tauri/src/proxy/providers/transform.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_codex_anthropic.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_codex_chat.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_codex_responses_namespace.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_codex_responses_xai_sanitize.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_gemini.rs": "protocol-transform",
    "src-tauri/src/proxy/providers/transform_responses.rs": "protocol-transform",
}

EXCLUSION_REASONS = {
    "excluded-auth": (
        "Credential extraction, OAuth/token handling, and upstream authentication "
        "headers remain owned by Nyro provider adapters and do not assert protocol "
        "wire conversion."
    ),
    "excluded-config": (
        "Proxy/provider configuration serialization and adapter construction remain "
        "owned by Nyro's configuration layer and do not assert protocol conversion."
    ),
    "excluded-failover": (
        "Retry, provider-health, circuit-breaker, and failover lifecycle remain owned "
        "by Nyro's dispatcher and are outside the protocol conversion closure."
    ),
    "excluded-forwarding": (
        "Private-field filtering and local proxy body/header override mechanics are "
        "forwarding policy owned by Nyro, not protocol wire conversion."
    ),
    "excluded-http-client": (
        "Proxy construction and generic HTTP buffering/body-limit behavior remain "
        "owned by Nyro's HTTP client and do not define protocol conversion."
    ),
    "excluded-observability": (
        "Retry/cache diagnostic wording and trace hashing remain owned by Nyro "
        "observability and do not change protocol wire input or output."
    ),
    "excluded-optimizer": (
        "The optional cache, thinking, media, or Copilot optimizer/rectifier is "
        "explicitly excluded from the cc-switch wire-parity conversion closure."
    ),
    "excluded-provider-management": (
        "Provider adapter selection, editable endpoint metadata, and provider-specific "
        "management remain owned by Nyro and do not define a wire transformation."
    ),
    "excluded-routing": (
        "Provider, URL, endpoint, and model routing remain owned by Nyro's existing "
        "router/dispatcher and are outside the wire-parity conversion layer."
    ),
    "excluded-usage": (
        "Usage pricing, deduplication, logging, notification, and persistence remain "
        "owned by Nyro's usage subsystem and do not assert wire conversion behavior."
    ),
}

FORWARDER_PROTOCOL_SSE = {
    "streaming_success_primes_first_chunk_and_replays_it",
    "streaming_first_chunk_error_is_retryable_before_success_record",
    "responses_stream_start_semantic_failure_is_retryable",
    "responses_stream_start_accepts_unlabelled_whole_json",
    "force_identity_for_stream_flag_requests",
    "force_identity_for_gemini_stream_endpoints",
    "streaming_request_detects_gemini_sse_without_body_stream_flag",
    "force_identity_for_sse_accept_header",
}
FORWARDER_PROTOCOL_TRANSPORT = {
    "non_streaming_requests_allow_automatic_compression",
}
FORWARDER_PROTOCOL_PROFILE = {
    "prepend_claude_code_system_prompt_from_string",
    "prepend_claude_code_system_prompt_when_absent",
    "prepend_claude_code_system_prompt_is_idempotent",
}
FORWARDER_PROTOCOL_ERROR = {
    "codex_anthropic_2xx_error_envelope_is_detected_for_failover",
    "responses_2xx_failure_is_detected_for_failover",
    "invalid_client_history_is_not_retryable",
}
FORWARDER_OBSERVABILITY = {
    "single_provider_retryable_log_uses_single_provider_code",
    "multi_provider_retryable_log_keeps_failover_wording",
    "single_provider_has_no_terminal_all_failed_log",
    "multi_provider_terminal_log_contains_last_error_summary",
    "summarize_text_for_log_collapses_whitespace_and_truncates",
    "canonical_json_sorts_object_keys_for_cache_trace_hashes",
}
FORWARDER_FORWARDING = {
    "prepare_upstream_request_body_filters_private_fields_and_canonicalizes_order",
    "local_proxy_body_overrides_deep_merge_final_body_without_stream",
    "local_proxy_header_overrides_replace_allowed_headers_only",
    "local_proxy_header_overrides_are_skipped_for_copilot",
}
FORWARDER_FAILOVER = {
    "non_streaming_success_is_buffered_before_marking_provider_successful",
    "non_streaming_body_read_error_is_retryable_before_success_record",
}
FORWARDER_AUTH = {
    "managed_account_upstream_rejects_proxy_managed_placeholder_header",
    "codex_oauth_upstream_rejects_proxy_managed_placeholder_header",
    "non_managed_upstream_allows_proxy_managed_placeholder_guard",
    "exact_header_case_preserved_for_native_claude_only",
    "exact_header_case_skipped_for_codex_oauth_and_copilot",
    "official_codex_auth_failures_are_not_retryable",
    "xai_oauth_token_auth_failures_are_not_retryable",
    "official_codex_rejects_stale_proxy_placeholder_with_restart_hint",
}

CLAUDE_PROTOCOL_EXACT = {
    "xai_oauth_invariants_ignore_editable_format_and_base_url",
}
CLAUDE_PROFILE_EXACT = {
    "test_needs_transform",
    "test_github_copilot_needs_transform",
}
CLAUDE_PROFILE_PREFIXES = (
    "test_deepseek_anthropic_",
    "test_kimi_anthropic_",
    "test_generic_anthropic_",
    "test_deepseek_official_",
    "test_non_deepseek_endpoint_",
    "test_normalize_messages_",
)
CODEX_PROTOCOL_PREFIXES = (
    "prompt_cache_",
    "test_uses_anthropic_",
    "test_codex_provider_uses_chat_completions_",
    "test_resolve_codex_chat_reasoning_",
)
CODEX_PROTOCOL_EXACT = {
    "test_anthropic_false_for_chat_and_responses",
    "test_anthropic_and_chat_are_mutually_exclusive",
    "test_should_convert_responses_to_anthropic_path_guard",
    "xai_oauth_pins_native_responses_catalog_profile",
    "namespace_flatten_gate_only_fires_for_xai_oauth",
}
CODEX_ROUTING_EXACT = {
    "test_resolve_catalog_profile_matches_router",
    "test_apply_codex_upstream_model_preserves_one_m_catalog_model",
    "test_apply_codex_chat_upstream_model_uses_provider_config_model",
    "test_apply_codex_chat_upstream_model_preserves_catalog_model_selection",
}
CODEX_AUTH_EXACT = {
    "xai_oauth_invariants_ignore_editable_base_url_and_auth",
}

EXCLUDED_MODULES = {
    "body_filter.rs": "excluded-forwarding",
    "cache_injector.rs": "excluded-optimizer",
    "circuit_breaker.rs": "excluded-failover",
    "copilot_optimizer.rs": "excluded-optimizer",
    "gemini_url.rs": "excluded-routing",
    "handler_context.rs": "excluded-routing",
    "http_client.rs": "excluded-http-client",
    "hyper_client.rs": "excluded-http-client",
    "media_sanitizer.rs": "excluded-optimizer",
    "model_mapper.rs": "excluded-routing",
    "provider_router.rs": "excluded-routing",
    "thinking_budget_rectifier.rs": "excluded-optimizer",
    "thinking_optimizer.rs": "excluded-optimizer",
    "thinking_rectifier.rs": "excluded-optimizer",
    "types.rs": "excluded-config",
    "providers/auth.rs": "excluded-auth",
    "providers/codex_oauth_auth.rs": "excluded-auth",
    "providers/copilot_auth.rs": "excluded-auth",
    "providers/copilot_model_map.rs": "excluded-routing",
    "providers/gemini.rs": "excluded-provider-management",
    "providers/mod.rs": "excluded-provider-management",
    "providers/xai_oauth_auth.rs": "excluded-auth",
    "usage/calculator.rs": "excluded-usage",
    "usage/logger.rs": "excluded-usage",
    "usage/parser.rs": "excluded-usage",
}

TEST_ATTRIBUTE_RE = re.compile(
    r"#\s*\[\s*(?:test|tokio\s*::\s*test)(?:\s*\([^\]]*\))?\s*\]"
)
FUNCTION_RE = re.compile(
    r"\b(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b"
)


@dataclasses.dataclass(frozen=True)
class RustTest:
    path: str
    name: str
    sha256: str

    @property
    def key(self) -> tuple[str, str]:
        return self.path, self.name


class AuditFailure(Exception):
    """Raised for input failures that prevent a meaningful audit."""


def run_git(source: pathlib.Path, *args: str) -> bytes:
    command = ["git", "-C", str(source), *args]
    completed = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise AuditFailure(f"{' '.join(command)} failed: {detail}")
    return completed.stdout


def resolve_commit(source: pathlib.Path, commit: str) -> str:
    return run_git(source, "rev-parse", f"{commit}^{{commit}}").decode().strip()


def git_blob(source: pathlib.Path, commit: str, path: str) -> bytes:
    return run_git(source, "show", f"{commit}:{path}")


def source_rust_files(source: pathlib.Path, commit: str) -> list[str]:
    raw = run_git(
        source,
        "ls-tree",
        "-r",
        "--name-only",
        "-z",
        commit,
        "--",
        SOURCE_ROOT,
    )
    return sorted(
        path.decode("utf-8")
        for path in raw.split(b"\0")
        if path and path.decode("utf-8").endswith(".rs")
    )


def _blank(out: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if out[index] not in "\r\n":
            out[index] = " "


def _raw_string_end(text: str, start: int) -> int | None:
    if start > 0 and (text[start - 1].isalnum() or text[start - 1] == "_"):
        return None
    match = re.match(r"(?:b|c)?r(?P<hashes>#{0,255})\"", text[start:])
    if not match:
        return None
    hashes = match.group("hashes")
    delimiter = '"' + hashes
    content_start = start + match.end()
    close = text.find(delimiter, content_start)
    return len(text) if close < 0 else close + len(delimiter)


def _quoted_end(text: str, quote: int, delimiter: str) -> int:
    index = quote + 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == delimiter:
            return index + 1
        if delimiter == "'" and text[index] in "\r\n":
            return quote
        index += 1
    return len(text) if delimiter == '"' else quote


def mask_rust_non_code(text: str) -> str:
    """Replace comments and literals with spaces while preserving offsets."""

    out = list(text)
    index = 0
    while index < len(text):
        if text.startswith("//", index):
            end = text.find("\n", index + 2)
            end = len(text) if end < 0 else end
            _blank(out, index, end)
            index = end
            continue
        if text.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(text) and depth:
                if text.startswith("/*", end):
                    depth += 1
                    end += 2
                elif text.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            _blank(out, index, end)
            index = end
            continue

        raw_end = _raw_string_end(text, index)
        if raw_end is not None:
            _blank(out, index, raw_end)
            index = raw_end
            continue

        quote_index = index
        if text[index] in {"b", "c"} and index + 1 < len(text) and text[index + 1] == '"':
            quote_index = index + 1
        if text[quote_index] == '"':
            end = _quoted_end(text, quote_index, '"')
            _blank(out, index, end)
            index = end
            continue

        char_quote = index
        if text[index] == "b" and index + 1 < len(text) and text[index + 1] == "'":
            char_quote = index + 1
        if text[char_quote] == "'":
            end = _quoted_end(text, char_quote, "'")
            if end != char_quote:
                _blank(out, index, end)
                index = end
                continue

        index += 1
    return "".join(out)


def extract_rust_tests(text: str, path: str) -> list[RustTest]:
    masked = mask_rust_non_code(text)
    tests: list[RustTest] = []
    consumed_until = -1
    for attribute in TEST_ATTRIBUTE_RE.finditer(masked):
        if attribute.start() < consumed_until:
            continue
        next_attribute = TEST_ATTRIBUTE_RE.search(masked, attribute.end())
        function = FUNCTION_RE.search(masked, attribute.end())
        if function is None or (
            next_attribute is not None and next_attribute.start() < function.start()
        ):
            raise AuditFailure(
                f"{path}: test attribute at byte {attribute.start()} has no test function"
            )
        body_start = masked.find("{", function.end())
        if body_start < 0:
            raise AuditFailure(f"{path}::{function.group('name')}: missing function body")
        depth = 0
        body_end = -1
        for index in range(body_start, len(masked)):
            if masked[index] == "{":
                depth += 1
            elif masked[index] == "}":
                depth -= 1
                if depth == 0:
                    body_end = index + 1
                    break
        if body_end < 0:
            raise AuditFailure(f"{path}::{function.group('name')}: unbalanced function body")
        item = text[attribute.start() : body_end].replace("\r\n", "\n").replace("\r", "\n")
        digest = hashlib.sha256((item.rstrip() + "\n").encode("utf-8")).hexdigest()
        tests.append(RustTest(path=path, name=function.group("name"), sha256=digest))
        consumed_until = body_end
    return tests


def scan_source(source: pathlib.Path, commit: str) -> tuple[list[RustTest], int]:
    tests: list[RustTest] = []
    files = source_rust_files(source, commit)
    for path in files:
        source_text = git_blob(source, commit, path).decode("utf-8")
        tests.extend(extract_rust_tests(source_text, path))
    return sorted(tests, key=lambda test: test.key), len(files)


def source_relative(path: str) -> str:
    prefix = SOURCE_ROOT + "/"
    if not path.startswith(prefix):
        raise AuditFailure(f"source path is outside {SOURCE_ROOT}: {path}")
    return path[len(prefix) :]


def classify_handler(name: str) -> str:
    if name.startswith("codex_proxy_") or "parse_error" in name or "diagnostic" in name:
        return "protocol-error"
    if "sse_to_response_value" in name or name.startswith("aggregated_"):
        return "protocol-aggregation"
    if "sse" in name or "streaming" in name:
        return "protocol-sse"
    if "force_streaming" in name:
        return "protocol-profile"
    return "protocol-transport"


def classify_source_test(path: str, name: str) -> str:
    if path in DIRECT_SOURCE_PATHS:
        return DIRECT_SOURCE_PATHS[path]

    relative = source_relative(path)
    if relative == "handlers.rs":
        return classify_handler(name)
    if relative == "content_encoding.rs":
        return "protocol-transport"
    if relative == "error_mapper.rs":
        return "protocol-error"
    if relative == "session.rs":
        return "protocol-state"
    if relative == "response_processor.rs":
        if name.startswith("test_log_usage_") or name.startswith("test_request_pricing_") or name.startswith("test_claude_desktop_"):
            return "excluded-usage"
        if name.startswith("test_strip_sse_"):
            return "protocol-sse"
        return "protocol-transport"
    if relative == "forwarder.rs":
        if name in FORWARDER_PROTOCOL_SSE:
            return "protocol-sse"
        if name in FORWARDER_PROTOCOL_TRANSPORT:
            return "protocol-transport"
        if name in FORWARDER_PROTOCOL_PROFILE:
            return "protocol-profile"
        if name in FORWARDER_PROTOCOL_ERROR:
            return "protocol-error"
        if name in FORWARDER_OBSERVABILITY:
            return "excluded-observability"
        if name in FORWARDER_FORWARDING or name == "codex_client_fingerprint_headers_are_dropped_for_anthropic_upstreams":
            return "excluded-forwarding"
        if name in FORWARDER_FAILOVER:
            return "excluded-failover"
        if name in FORWARDER_AUTH or name == "codex_oauth_session_headers_match_codex_cache_identity":
            return "excluded-auth"
        if (
            name.startswith("rewrite_")
            or name.startswith("append_query_")
            or name.startswith("build_gemini_")
            or name.startswith("resolve_gemini_")
            or name == "codex_anthropic_full_endpoint_guard_avoids_double_messages"
        ):
            return "excluded-routing"
        if name == "codex_anthropic_cache_is_default_on_but_honors_sub_switch":
            return "excluded-optimizer"
        if "copilot_detection" in name:
            return "excluded-provider-management"
        if name.startswith("dynamic_endpoint_") or name.startswith("prevention_") or name.startswith("reactive_"):
            return "excluded-optimizer"
        raise AuditFailure(f"unclassified forwarder test: {path}::{name}")
    if relative == "providers/claude.rs":
        if (
            name.startswith("test_transform_")
            or name.startswith("test_anthropic_messages_")
            or name.startswith("test_anthropic_system_")
            or name.startswith(CLAUDE_PROFILE_PREFIXES)
            or name in CLAUDE_PROTOCOL_EXACT
            or name in CLAUDE_PROFILE_EXACT
        ):
            return "protocol-profile"
        if "auth" in name or name.startswith("test_extract_") or name.startswith("test_get_auth_"):
            return "excluded-auth"
        if name.startswith("test_build_url_"):
            return "excluded-routing"
        return "excluded-provider-management"
    if relative == "providers/codex.rs":
        if name.startswith(CODEX_PROTOCOL_PREFIXES) or name in CODEX_PROTOCOL_EXACT:
            return "protocol-profile"
        if name in CODEX_ROUTING_EXACT or name.startswith("test_build_url"):
            return "excluded-routing"
        if name in CODEX_AUTH_EXACT or "auth" in name or name.startswith("test_extract_") or name.startswith("is_official_") or name.startswith("test_is_official_") or name.startswith("test_is_not_official_"):
            return "excluded-auth"
        return "excluded-provider-management"
    if relative in EXCLUDED_MODULES:
        return EXCLUDED_MODULES[relative]
    raise AuditFailure(f"unclassified source test: {path}::{name}")


def direct_target_path(source_path: str) -> str:
    relative = source_relative(source_path)
    return f"crates/nyro-ccswitch-compat/src/ported/{relative}"


def target_locator(path: str, name: str) -> str:
    return f"{path}::{name}"


def pending_mapping(classification: str, source_path: str) -> str:
    relative = source_relative(source_path)
    if relative == "handlers.rs" or classification in {
        "protocol-aggregation",
        "protocol-sse",
        "protocol-transport",
    }:
        destination = "crates/nyro-ccswitch-compat/src/transport.rs"
        focus = "transport/SSE classification and aggregation assertions"
    elif classification == "protocol-error":
        destination = "crates/nyro-ccswitch-compat/src/ported/error.rs or src/transport.rs"
        focus = "status, payload shape, diagnostics, and error normalization assertions"
    elif classification == "protocol-state":
        destination = "crates/nyro-ccswitch-compat/src/session.rs or src/state.rs"
        focus = "session identity and state-boundary assertions"
    elif classification == "protocol-profile":
        module = "claude.rs" if relative == "providers/claude.rs" else "codex.rs"
        destination = f"crates/nyro-ccswitch-compat/src/profile_rules/{module}"
        focus = "profile selection and request policy assertions"
    else:
        destination = "crates/nyro-ccswitch-compat"
        focus = "source assertion-level behavior"
    return (
        f"PENDING: no audited target test existed at inventory generation; map the {focus} "
        f"to an exact test in {destination} and replace this marker only after reviewing "
        "the source assertions."
    )


def make_inventory_entry(test: RustTest, commit: str) -> dict[str, str]:
    classification = classify_source_test(test.path, test.name)
    if test.path in DIRECT_SOURCE_PATHS:
        return {
            "source_commit": commit,
            "source_path": test.path,
            "source_test_name": test.name,
            "source_sha256": test.sha256,
            "classification": classification,
            "target_test": target_locator(direct_target_path(test.path), test.name),
            "target_sha256": "AUTO",
            "mapping": (
                "Direct source-module port; the target retains this test identity and "
                "its source assertions."
            ),
            "status": "migrated",
            "reason": "Direct mechanical port of the fixed source module.",
        }
    if classification in REQUIRED_CLASSIFICATIONS:
        reviewed = REVIEWED_MAPPINGS.get(test.key)
        if reviewed is None:
            target_test = ""
            mapping = pending_mapping(classification, test.path)
            reason = (
                "Incomplete until an exact compatibility target exists and its assertions "
                "have been audited against this source test."
            )
        else:
            target_test, mapping = reviewed
            reason = "Assertion-level mapping reviewed against the fixed source test."
        return {
            "source_commit": commit,
            "source_path": test.path,
            "source_test_name": test.name,
            "source_sha256": test.sha256,
            "classification": classification,
            "target_test": target_test,
            "target_sha256": "AUTO" if reviewed is not None else "",
            "mapping": mapping,
            "status": "mapped",
            "reason": reason,
        }
    if classification not in EXCLUSION_REASONS:
        raise AuditFailure(f"no exclusion policy for {test.path}::{test.name}: {classification}")
    reason = EXCLUSION_REASONS[classification]
    return {
        "source_commit": commit,
        "source_path": test.path,
        "source_test_name": test.name,
        "source_sha256": test.sha256,
        "classification": classification,
        "target_test": "",
        "target_sha256": "",
        "mapping": "Excluded from the cc-switch wire-parity compatibility target.",
        "status": "not-applicable",
        "reason": reason,
    }


def inventory_metadata(commit: str, test_count: int) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "source_project": "cc-switch",
        "source_commit": commit,
        "source_root": SOURCE_ROOT,
        "source_hash_algorithm": HASH_ALGORITHM,
        "source_test_count": test_count,
        "source_license_path": "LICENSE",
        "vendored_license_path": "THIRD_PARTY_LICENSES/cc-switch-MIT.txt",
        "target_locator_format": "repository-relative-file.rs::test_function_name",
    }


def toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def write_inventory(path: pathlib.Path, metadata: dict[str, Any], entries: list[dict[str, str]]) -> None:
    lines = [
        "# Generated audit inventory for the fixed cc-switch proxy test baseline.",
        "# Edit target_test/mapping/status deliberately; source fields are checker-owned.",
        "",
    ]
    metadata_order = [
        "schema_version",
        "source_project",
        "source_commit",
        "source_root",
        "source_hash_algorithm",
        "source_test_count",
        "source_license_path",
        "vendored_license_path",
        "target_locator_format",
    ]
    for key in metadata_order:
        value = metadata[key]
        rendered = str(value) if isinstance(value, int) else toml_string(str(value))
        lines.append(f"{key} = {rendered}")
    field_order = [
        "source_commit",
        "source_path",
        "source_test_name",
        "source_sha256",
        "classification",
        "target_test",
        "target_sha256",
        "mapping",
        "status",
        "reason",
    ]
    for entry in sorted(entries, key=lambda item: (item["source_path"], item["source_test_name"])):
        lines.extend(["", "[[test]]"])
        for key in field_order:
            lines.append(f"{key} = {toml_string(str(entry.get(key, '')))}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def load_inventory(path: pathlib.Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise AuditFailure(f"cannot read inventory {path}: {error}") from error
    entries = data.pop("test", None)
    if not isinstance(entries, list):
        raise AuditFailure(f"{path}: expected one or more [[test]] tables")
    if not all(isinstance(entry, dict) for entry in entries):
        raise AuditFailure(f"{path}: every [[test]] value must be a table")
    return data, entries


def parse_target_locator(locator: str) -> tuple[str, str] | None:
    if "::" not in locator:
        return None
    path, name = locator.rsplit("::", 1)
    if not path.endswith(".rs") or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
        return None
    candidate = pathlib.PurePosixPath(path)
    if candidate.is_absolute() or ".." in candidate.parts:
        return None
    return path, name


def iter_target_rust_files(target_root: pathlib.Path) -> Iterable[pathlib.Path]:
    compat = target_root / "crates/nyro-ccswitch-compat"
    if compat.exists():
        yield from compat.rglob("*.rs")
    # The dispatcher integration surface: profile selection, stream decisions,
    # error envelopes, and the cc-switch parity round-trip harness live here.
    dispatcher_compat = target_root / "crates/nyro-core/src/proxy/dispatcher/compat.rs"
    if dispatcher_compat.exists():
        yield dispatcher_compat
    parity_test = target_root / "crates/nyro-core/tests/cc_switch_parity.rs"
    if parity_test.exists():
        yield parity_test


def scan_targets(target_root: pathlib.Path) -> dict[tuple[str, str], RustTest]:
    targets: dict[tuple[str, str], RustTest] = {}
    for path in sorted(set(iter_target_rust_files(target_root))):
        relative = path.relative_to(target_root).as_posix()
        text = path.read_text(encoding="utf-8")
        for test in extract_rust_tests(text, relative):
            if test.key in targets:
                raise AuditFailure(f"duplicate target test identity: {relative}::{test.name}")
            targets[test.key] = test
    return targets


def choose_target_candidate(
    entry: dict[str, Any], targets: dict[tuple[str, str], RustTest]
) -> RustTest | None:
    source_name = str(entry.get("source_test_name", ""))
    candidates = [test for test in targets.values() if test.name == source_name]
    if not candidates:
        return None
    source_path = str(entry.get("source_path", ""))
    preferred_path = direct_target_path(source_path) if source_path in DIRECT_SOURCE_PATHS else ""
    preferred = [test for test in candidates if test.path == preferred_path]
    if len(preferred) == 1:
        return preferred[0]
    source_basename = pathlib.PurePosixPath(source_path).name
    basename_matches = [
        test for test in candidates if pathlib.PurePosixPath(test.path).name == source_basename
    ]
    if len(basename_matches) == 1:
        return basename_matches[0]
    return candidates[0] if len(candidates) == 1 else None


def apply_exact_target_updates(
    entries: list[dict[str, Any]],
    targets: dict[tuple[str, str], RustTest],
    updates: list[str],
) -> int:
    entries_by_key = {
        (str(entry.get("source_path", "")), str(entry.get("source_test_name", ""))): entry
        for entry in entries
    }
    updated = 0
    for specification in updates:
        if "=" not in specification:
            raise AuditFailure(f"invalid --update-target value (missing '='): {specification}")
        source_locator, target_text = specification.split("=", 1)
        source_key = parse_target_locator(source_locator)
        target_key = parse_target_locator(target_text)
        if source_key is None:
            raise AuditFailure(f"invalid source locator in --update-target: {source_locator}")
        if target_key is None:
            raise AuditFailure(f"invalid target locator in --update-target: {target_text}")
        entry = entries_by_key.get(source_key)
        if entry is None:
            raise AuditFailure(f"unknown source test in --update-target: {source_locator}")
        if entry.get("status") == "not-applicable":
            raise AuditFailure(f"cannot target an excluded source test: {source_locator}")
        target = targets.get(target_key)
        if target is None:
            raise AuditFailure(f"target test does not exist: {target_text}")
        entry["target_test"] = target_text
        entry["target_sha256"] = target.sha256
        updated += 1
    return updated


def update_target_locators(
    entries: list[dict[str, Any]], targets: dict[tuple[str, str], RustTest]
) -> tuple[int, list[str]]:
    updated = 0
    ambiguous: list[str] = []
    by_name = collections.Counter(test.name for test in targets.values())
    for entry in entries:
        if entry.get("status") == "not-applicable":
            continue
        current = parse_target_locator(str(entry.get("target_test", "")))
        if current in targets:
            if not str(entry.get("target_sha256", "")):
                entry["target_sha256"] = targets[current].sha256
                updated += 1
            continue
        candidate = choose_target_candidate(entry, targets)
        if candidate is None:
            source_name = str(entry.get("source_test_name", ""))
            if by_name[source_name] > 1:
                ambiguous.append(f"{entry.get('source_path')}::{source_name}")
            continue
        entry["target_test"] = target_locator(candidate.path, candidate.name)
        # Discovery is not an assertion review. Deliberately leave this blank so
        # --require-complete continues to fail until a reviewer locks the target.
        entry["target_sha256"] = ""
        updated += 1
    return updated, ambiguous


def is_pending(entry: dict[str, Any], targets: dict[tuple[str, str], RustTest]) -> tuple[bool, str]:
    locator_text = str(entry.get("target_test", ""))
    parsed = parse_target_locator(locator_text)
    mapping = str(entry.get("mapping", ""))
    if not locator_text:
        return True, "no target_test locator"
    if parsed is None:
        return True, "invalid target_test locator"
    target = targets.get(parsed)
    if target is None:
        return True, "target test does not exist"
    target_hash = str(entry.get("target_sha256", ""))
    if not target_hash:
        return True, "target_sha256 is not locked after assertion review"
    if target_hash != target.sha256:
        return True, "target_sha256 is stale"
    if "PENDING" in mapping.upper():
        return True, "mapping still carries an explicit PENDING marker"
    if len(mapping.strip()) < 30:
        return True, "mapping does not describe assertion-level coverage"
    return False, ""


def audit(
    *,
    source: pathlib.Path,
    commit: str,
    inventory_path: pathlib.Path,
    target_root: pathlib.Path,
    require_complete: bool,
    initialize: bool,
    exact_target_updates: list[str],
    update_targets: bool,
) -> int:
    resolved_commit = resolve_commit(source, commit)
    source_tests, source_file_count = scan_source(source, resolved_commit)
    source_by_key = {test.key: test for test in source_tests}
    if len(source_by_key) != len(source_tests):
        duplicates = [
            key
            for key, count in collections.Counter(test.key for test in source_tests).items()
            if count > 1
        ]
        raise AuditFailure(f"duplicate source tests discovered: {duplicates[:10]}")

    if initialize:
        if inventory_path.exists():
            raise AuditFailure(f"refusing to overwrite existing inventory: {inventory_path}")
        entries = [make_inventory_entry(test, resolved_commit) for test in source_tests]
        initial_targets = scan_targets(target_root)
        for entry in entries:
            parsed_target = parse_target_locator(str(entry.get("target_test", "")))
            if entry.get("target_sha256") == "AUTO" and parsed_target in initial_targets:
                entry["target_sha256"] = initial_targets[parsed_target].sha256
        inventory_path.parent.mkdir(parents=True, exist_ok=True)
        write_inventory(
            inventory_path,
            inventory_metadata(resolved_commit, len(source_tests)),
            entries,
        )
        print(f"Initialized {inventory_path} with {len(entries)} source tests.")

    metadata, entries = load_inventory(inventory_path)
    targets = scan_targets(target_root)
    if exact_target_updates:
        updated = apply_exact_target_updates(entries, targets, exact_target_updates)
        write_inventory(inventory_path, metadata, entries)
        print(f"Applied {updated} exact target update(s) to {inventory_path}.")
        metadata, entries = load_inventory(inventory_path)
    if update_targets:
        updated, ambiguous = update_target_locators(entries, targets)
        for entry in entries:
            if "PENDING" in str(entry.get("mapping", "")).upper():
                continue
            parsed_target = parse_target_locator(str(entry.get("target_test", "")))
            if parsed_target in targets:
                entry["target_sha256"] = targets[parsed_target].sha256
        write_inventory(inventory_path, metadata, entries)
        print(f"Updated {updated} target_test locator(s) in {inventory_path}.")
        if ambiguous:
            print(f"Left {len(ambiguous)} ambiguous same-name target(s) unchanged.")
        metadata, entries = load_inventory(inventory_path)

    errors: list[str] = []
    expected_metadata = inventory_metadata(resolved_commit, len(source_tests))
    for key, expected in expected_metadata.items():
        actual = metadata.get(key)
        if actual != expected:
            errors.append(f"metadata {key}: expected {expected!r}, found {actual!r}")

    license_path = target_root / str(metadata.get("vendored_license_path", ""))
    try:
        vendored_license = license_path.read_bytes()
    except OSError as error:
        errors.append(f"cannot read vendored source license {license_path}: {error}")
    else:
        source_license = git_blob(
            source, resolved_commit, str(metadata.get("source_license_path", "LICENSE"))
        )
        if vendored_license != source_license:
            errors.append(
                f"vendored source license differs byte-for-byte from {resolved_commit}:LICENSE"
            )

    required_fields = {
        "source_commit",
        "source_path",
        "source_test_name",
        "source_sha256",
        "classification",
        "target_test",
        "target_sha256",
        "mapping",
        "status",
        "reason",
    }
    inventory_by_key: dict[tuple[str, str], dict[str, Any]] = {}
    duplicate_keys: list[tuple[str, str]] = []
    status_counts: collections.Counter[str] = collections.Counter()
    classification_counts: collections.Counter[str] = collections.Counter()
    pending: list[tuple[str, str, str]] = []

    for index, entry in enumerate(entries, start=1):
        missing_fields = sorted(required_fields - set(entry))
        if missing_fields:
            errors.append(f"entry {index}: missing fields {', '.join(missing_fields)}")
        path = str(entry.get("source_path", ""))
        name = str(entry.get("source_test_name", ""))
        key = (path, name)
        if key in inventory_by_key:
            duplicate_keys.append(key)
        else:
            inventory_by_key[key] = entry

        status = entry.get("status")
        if status not in ALLOWED_STATUSES:
            errors.append(f"{path}::{name}: invalid status {status!r}")
        else:
            status_counts[str(status)] += 1
        target_hash = str(entry.get("target_sha256", ""))
        if target_hash and not re.fullmatch(r"[0-9a-f]{64}", target_hash):
            errors.append(f"{path}::{name}: target_sha256 is not a lowercase SHA-256")
        classification = str(entry.get("classification", ""))
        classification_counts[classification] += 1

        source_test = source_by_key.get(key)
        if source_test is None:
            continue
        if entry.get("source_commit") != resolved_commit:
            errors.append(
                f"{path}::{name}: source_commit is {entry.get('source_commit')!r}, "
                f"expected {resolved_commit}"
            )
        if entry.get("source_sha256") != source_test.sha256:
            errors.append(
                f"{path}::{name}: stale source_sha256; expected {source_test.sha256}"
            )
        expected_classification = classify_source_test(path, name)
        if classification != expected_classification:
            errors.append(
                f"{path}::{name}: classification {classification!r} does not match "
                f"policy-derived {expected_classification!r}"
            )

        target_text = str(entry.get("target_test", ""))
        mapping = str(entry.get("mapping", ""))
        reason = str(entry.get("reason", ""))
        if status == "not-applicable":
            if expected_classification in REQUIRED_CLASSIFICATIONS:
                errors.append(
                    f"{path}::{name}: forbidden not-applicable classification "
                    f"for {expected_classification}"
                )
            if not expected_classification.startswith("excluded-"):
                errors.append(
                    f"{path}::{name}: not-applicable entry lacks an approved exclusion class"
                )
            if target_text:
                errors.append(f"{path}::{name}: not-applicable entry must not name a target")
            expected_reason = EXCLUSION_REASONS.get(expected_classification)
            if expected_reason is None or reason != expected_reason:
                errors.append(
                    f"{path}::{name}: exclusion reason is missing, generic, or stale"
                )
            if "excluded" not in mapping.lower():
                errors.append(f"{path}::{name}: exclusion mapping must be explicit")
        elif status in {"migrated", "mapped"}:
            if expected_classification not in REQUIRED_CLASSIFICATIONS:
                errors.append(
                    f"{path}::{name}: covered status conflicts with exclusion policy "
                    f"{expected_classification}"
                )
            if status == "migrated":
                if path not in DIRECT_SOURCE_PATHS:
                    errors.append(
                        f"{path}::{name}: migrated is reserved for direct source-module ports"
                    )
                expected_target = target_locator(direct_target_path(path), name)
                if target_text != expected_target:
                    errors.append(
                        f"{path}::{name}: migrated target must be {expected_target!r}"
                    )
            incomplete, incomplete_reason = is_pending(entry, targets)
            if incomplete:
                pending.append((path, name, incomplete_reason))

    if duplicate_keys:
        for path, name in duplicate_keys[:20]:
            errors.append(f"duplicate inventory entry: {path}::{name}")
        if len(duplicate_keys) > 20:
            errors.append(f"... and {len(duplicate_keys) - 20} more duplicate entries")

    source_keys = set(source_by_key)
    inventory_keys = set(inventory_by_key)
    missing_entries = sorted(source_keys - inventory_keys)
    stale_entries = sorted(inventory_keys - source_keys)
    for path, name in missing_entries[:20]:
        errors.append(f"new or missing inventory source test: {path}::{name}")
    if len(missing_entries) > 20:
        errors.append(f"... and {len(missing_entries) - 20} more missing source tests")
    for path, name in stale_entries[:20]:
        errors.append(f"removed or renamed source test remains in inventory: {path}::{name}")
    if len(stale_entries) > 20:
        errors.append(f"... and {len(stale_entries) - 20} more stale source tests")

    complete_covered = status_counts["migrated"] + status_counts["mapped"] - len(pending)
    print(f"Source commit: {resolved_commit}")
    print(f"Source tests: {len(source_tests)} across {source_file_count} proxy Rust files")
    print(f"Inventory entries: {len(entries)}")
    print(
        "Statuses: "
        + ", ".join(
            f"{status}={status_counts[status]}"
            for status in ("migrated", "mapped", "not-applicable")
        )
    )
    print(
        f"Coverage: complete={complete_covered}, pending={len(pending)}, "
        f"excluded={status_counts['not-applicable']}"
    )
    print(f"Discovered target tests: {len(targets)}")

    if errors:
        print(f"Integrity: FAILED ({len(errors)} issue(s))", file=sys.stderr)
        for error in errors[:50]:
            print(f"  - {error}", file=sys.stderr)
        if len(errors) > 50:
            print(f"  - ... and {len(errors) - 50} more", file=sys.stderr)
        return 1

    print("Integrity: OK")
    if pending:
        print(f"Completeness: INCOMPLETE ({len(pending)} mapping(s) pending)")
        for path, name, why in pending[:15]:
            print(f"  - {path}::{name}: {why}")
        if len(pending) > 15:
            print(f"  - ... and {len(pending) - 15} more")
        if require_complete:
            print(
                "--require-complete rejected pending mappings; implement and audit every target.",
                file=sys.stderr,
            )
            return 1
    else:
        print("Completeness: COMPLETE")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    repository_root = pathlib.Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--commit", required=True)
    parser.add_argument(
        "--inventory",
        type=pathlib.Path,
        default=repository_root / "crates/nyro-ccswitch-compat/tests/parity_inventory.toml",
    )
    parser.add_argument("--target-root", type=pathlib.Path, default=repository_root)
    parser.add_argument("--require-complete", action="store_true")
    parser.add_argument(
        "--initialize",
        action="store_true",
        help="create a deterministic inventory; refuses to overwrite an existing file",
    )
    parser.add_argument(
        "--update-target",
        action="append",
        default=[],
        metavar="SOURCE_PATH::SOURCE_TEST=TARGET_FILE::TARGET_TEST",
        help=(
            "set an exact target locator and hash while retaining the existing mapping; "
            "repeat for multiple entries"
        ),
    )
    parser.add_argument(
        "--update-targets",
        action="store_true",
        help="discover unique same-name targets without clearing PENDING reviews",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        return audit(
            source=args.source.resolve(),
            commit=args.commit,
            inventory_path=args.inventory.resolve(),
            target_root=args.target_root.resolve(),
            require_complete=args.require_complete,
            initialize=args.initialize,
            exact_target_updates=args.update_target,
            update_targets=args.update_targets,
        )
    except (AuditFailure, OSError, UnicodeDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
