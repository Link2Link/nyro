# Experimental native Cursor AgentService provider

[简体中文](<cursor-provider.CN.md>)

## Status and scope

Nyro's Cursor integration is a **native Rust, outbound-only AgentService adapter**.
It does not run Cursor CLI, Node/Bun, a second gateway, local shell commands, or
workspace tools. It is not an OpenAI-compatible upstream endpoint.

The implementation is experimental. Offline compilation and protocol tests have
passed during development; **no real Cursor credential, login, catalog request,
model generation, tool round-trip, or subscription/billing verification has been
performed**. Public source inspection is not evidence of production compatibility.
The pinned reference may differ from the service deployed for a particular account.

Optional rich model-catalog enrichment and account-specific reasoning routes are
implemented and tested with offline fixtures. They join only exact, entitled
legacy IDs and keep non-reasoning cost/context parameters fixed. They have not
been verified against a real account.

## Architecture

```text
OpenAI / Anthropic / Gemini compatible client request
    -> existing ingress decoder -> AiRequest
    -> outbound protocol negotiation: cursor-agent/run/v1
    -> Cursor Vendor::start_ir
       -> credential + account-usable model discovery
       -> request/history capability validation
       -> pure CursorSession
          <-> native HTTP/2 duplex transport
          <-> api2.cursor.sh/agent.v1.AgentService/Run
       -> AiStreamDelta
    -> existing accumulator / ingress response encoder
    -> buffered JSON or client-facing stream
```

The registry declares a native egress protocol; it does not expose a Cursor
HTTP ingress route or register a JSON codec pretending to speak AgentService.
The legacy JSON `build_request` / `parse_response` path explicitly rejects use.
Transport and provider policy remain separate:

| Boundary | Responsibility |
|---|---|
| [Protocol schema](<../../crates/nyro-core/src/provider/cursor/wire.rs>) | Minimal handwritten `prost` messages with verified field numbers; no `protoc` build dependency |
| [Connect framing](<../../crates/nyro-core/src/provider/cursor/framing.rs>) | Incremental envelopes, gzip, trailer errors and hard size bounds |
| [History](<../../crates/nyro-core/src/provider/cursor/history.rs>) | Caller IR replay, SHA-256 memory blobs, system rules, MCP schemas/Values |
| [Session](<../../crates/nyro-core/src/provider/cursor/session.rs>) | Pure state machine: outgoing frames, IR events, verified completion or explicit failure |
| [Catalog](<../../crates/nyro-core/src/provider/cursor/catalog.rs>) | Authoritative account-usable IDs and model-route data |
| [Runtime](<../../crates/nyro-core/src/provider/cursor/runtime.rs>) | Auth headers, discovery, bounded queues, heartbeat, cancellation and resource ownership |
| [Duplex transport](<../../crates/nyro-core/src/proxy/client/duplex.rs>) | Required HTTP/2, upload body stream, response data **and trailers**, deadlines |
| [Native IR response path](<../../crates/nyro-core/src/proxy/dispatcher/ir_response.rs>) | Response formatting, terminal/error accounting and limitation headers |
| [OAuth driver](<../../crates/nyro-core/src/auth/drivers/cursor.rs>) | Native PKCE/poll/refresh flow; no browser credential scraping |

## Service and catalog contracts

- Fixed service origin: `https://api2.cursor.sh`. This is not a configurable
  third-party relay URL.
- Generation: `/agent.v1.AgentService/Run`, HTTP/2 duplex,
  `application/connect+proto`.
- Baseline discovery: `/agent.v1.AgentService/GetUsableModels`,
  `application/proto`, **bare protobuf body**, not Connect-framed protobuf.
- Discovery is account/credential specific. The runtime caches the decoded catalog
  briefly under provider identity, a credential fingerprint and client version;
  it does not cache a live conversation or a bearer token as a map key.
- Exact IDs remain exact. No model-family aliases, effort-suffix rewriting or
  fallback to a different model is inferred by the baseline.
- A model route can represent separate legacy `modelDetails.modelId`,
  `requestedModel.modelId`, parameters and max-mode status. A richer route must
  come from authoritative server metadata, not name heuristics.
- Client identity follows the pinned reference build
  `cli-2026.09.02-c22c1a3`; it is a protocol header, not an installed CLI dependency.

An administrator's provider probe uses the same native Run path. Running a probe
is a **real upstream call** that may consume subscription quota; it was not run
as part of the offline implementation verification.

## Request and history semantics

Each client request creates a fresh session and conversation ID, rebuilt from the
request's complete history. There is no cross-request live HTTP session, hidden
checkpoint resume cache, local tool executor or background agent loop.

Historical prompt entries are JSON blobs in Cursor's internal message shape,
referenced by SHA-256 IDs in `rootPromptMessagesJson`. Typed display turns reference
serialized user messages and assistant/thinking/MCP steps through the same memory
store. Tool calls retain their IDs, names and JSON arguments. Paired results retain
empty content and explicit errors. Orphan, duplicate and unanswered historical
calls are rejected instead of being transformed into invented assistant text.

A trailing user message is sent as `userMessageAction` and excluded from historical
prompt/turn entries, including an explicitly empty user message. Trailing tool
results use `resumeAction` with the **entire rebuilt history**, not a cached active
run. No XML tool-result wrappers or fabricated assistant acknowledgements are used.

System text is caller configuration, sent both as historical system entries and
as global USER rules in `RequestContext`. Paths on these rules are protocol labels,
not files read from or written to the host. Cursor controls the actual system
prompt; caller configuration remains **best effort**. The allowlisted upstream
`custom_system_prompt` override is not claimed.

## Tools and local handoff

Only declared caller tools are advertised as MCP definitions. Schema bytes and
each argument-map value use `google.protobuf.Value`, not JSON bytes or a bare
`Struct`. Nulls, booleans, arrays, nested objects and empty objects retain their
meaning. Malformed Values, missing kinds, non-finite numbers, unrepresentable
integer precision and non-object argument JSON fail explicitly.

Tool names are not silently rewritten or removed because they resemble native
names such as `read` or `bash`. They are still caller-owned MCP tools. `none`
advertises no tools; named choice advertises only the selected declared tool.
Required/named choice cannot silently finish as a normal text answer without a
handoff. Full JSON Schema validation is **not** claimed by this adapter.

At the first verified tool call the session emits:

```text
ToolCallStart -> ToolCallDelta(authoritative arguments) -> ToolCallComplete
             -> Done(tool_use) -> Terminal(ToolHandoff)
```

Argument previews are buffered until final typed arguments arrive, so replaced or
incomplete preview JSON is never forwarded as a completed call. The runtime then
closes the upstream run. `ToolHandoff` is a deliberate **local** terminal, not proof
that Cursor sent `turnEnded`. The caller executes the tool and makes a new HTTP
request containing the full updated history. The adapter does not execute tools,
keep a live run awaiting their results, or initiate parallel tool execution.

Known native shell/filesystem operations are answered with typed rejection/error
responses. Other recognized native exec families receive `ExecClientThrow` and
stream-close, followed by an explicit unsupported failure. Unknown operations,
unimplemented approval queries, subagent workflows and native streaming are not
silently acknowledged or treated as completed work.

## Completion, errors and resource bounds

Normal success requires all of the following:

1. An authoritative `turnEnded` update.
2. A clean Connect end envelope or explicit successful transport status trailer.
3. Complete framing, no incomplete tool call and no unsupported native operation.
4. HTTP response EOF **after trailers have been checked**.

Text output, HTTP 200, `stepCompleted`, or bare EOF alone is not completion. A late
error trailer wins over earlier `turnEnded`. Truncation, invalid protobuf, bad gzip,
unknown operations, missing blobs and SHA-256 mismatches are typed failures.

| Bound | Baseline limit |
|---|---:|
| Compressed Connect payload | 4 MiB |
| Inflated or uncompressed Connect payload | 8 MiB |
| Per-session blob data total | 32 MiB |
| Individual blob | 8 MiB minus envelope headroom |

The decoder accepts split headers/bodies and multiple envelopes in one HTTP data
chunk without trusting an unchecked advertised length. Transport preserves trailers
rather than flattening the response into a byte-only stream. Upload/event queues
are bounded; cancellation, client disconnect or deadline expiration must drop the
session resources. No automatic replay of an already submitted Run is safe merely
because an upstream response failed.

## Capability limitations and usage

The native response path advertises:

```http
x-nyro-provider-limitations: system_best_effort,token_budget_not_enforced,usage_unknown
```

- Positive `max_tokens` is accepted for clients that require the field, but the
  native Run mapping does **not enforce** that budget. Zero is invalid.
- Plain text response format is supported. Structured JSON/schema output and
  unverified sampling controls are rejected rather than silently ignored.
- Baseline history is text/thinking/function-call/result only. Multimodal and
  server-tool blocks, custom/namespaced tools and interactive agent operations
  are explicitly unsupported.
- Explicit reasoning requests require an authoritative catalog route; absence of
  rich metadata is not permission to infer a route from a model name. A client
  thinking budget (`budget_tokens` / effort `budget`) has no Cursor wire
  equivalent: it is translated to a qualitative tier using the gateway-wide
  threshold table shared with the Claude 5.5 relay rewrite (`>10240→max,
  >8192→xhigh, >1024→high, >0→medium`; a budget-derived tier overrides a
  concurrently supplied effort). The translated tier must still be declared by
  authoritative lane metadata or the request fails closed. Reasoning
  display/summary modes remain rejected.
- Only authoritative `TurnEnded` counters are used for IR usage. Omitted counters
  remain unknown in session state. `tokenDelta` estimates and checkpoint context
  occupancy are not completion-token estimates. The legacy integer IR can encode
  a complete input/output pair but cannot express every absent bucket; absence of
  a Usage event must not be sold as measured zero usage.

## Verification and follow-up

Offline tests exercise framing split/sticky/gzip/limits/errors, golden protobuf
bytes, exact model IDs, memory KV integrity, history replay, empty/error results,
request-context exchange, text/thinking events, usage, terminal timing, partial
arguments, declared-tool checks and native rejection. Integration also includes
protocol negotiation, OAuth and dispatcher tests. The integrating change should
run the following after all sibling changes are merged into the working tree:

```bash
cargo test -p nyro-core --lib cursor
cargo check -p nyro-core
```

These tests do not replace account-authorized live verification. Before broader
rollout, verify OAuth/refresh, model discovery, text streaming, caller tool handoff
and continuation, proxy/HTTP2 behavior, cancellation, trailers and observed billing
against a test account. Rich AvailableModels enrichment, additional modalities and
native-agent operations are separate capabilities, not assumed baseline support.

## Protocol provenance

The authoritative implementation reference is the MIT-licensed **oh-my-pi** commit
[`5d0aaffaf1f987503ce129a642fb6e30339615af`](<https://github.com/can1357/oh-my-pi/tree/5d0aaffaf1f987503ce129a642fb6e30339615af>).
The pinned schemas, public source links, adaptation notes and complete MIT notice
are recorded in [protocol sources](<../../crates/nyro-core/src/provider/cursor/PROTOCOL_SOURCES.md>).
No AGPL Astro code was used.
