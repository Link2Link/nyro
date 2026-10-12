# Native Cursor protocol sources and boundary

## Pinned public reference

This implementation was written against **can1357/oh-my-pi commit
`5d0aaffaf1f987503ce129a642fb6e30339615af`** (resolved from public `main`/HEAD).
The `master` branch was not available at inspection time. Only public GitHub
source was fetched; no real account, token, Cursor upstream request, Cursor CLI,
Node process, or AGPL Astro source was used.

- [Provider implementation](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/packages/ai/src/providers/cursor.ts): prompt/history blobs, content hashes, request-context handshake, MCP Value encoding, request model tuple, and authoritative turn usage.
- [Agent schema](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/packages/ai/src/providers/cursor/proto/agent.proto): authoritative field numbers and wire types transcribed as a minimal handwritten `prost` subset.
- [Compiled declaration subset](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/packages/catalog/src/discovery/cursor-proto.ts): cross-check for schema declarations consumed by the reference client.
- [Rich catalog schema](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/packages/catalog/src/discovery/cursor-models.proto): optional AvailableModels enrichment joins exact legacy slugs onto the authoritative GetUsableModels IDs; it does not expand account entitlement.
- [Wire constants](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/packages/catalog/src/wire/cursor.ts): host, paths, and client build string.
- [Pinned MIT license](https://github.com/can1357/oh-my-pi/blob/5d0aaffaf1f987503ce129a642fb6e30339615af/LICENSE).

## First-version contract

- Transport owns one HTTP/2 duplex request to `https://api2.cursor.sh/agent.v1.AgentService/Run`, Connect envelopes, authorization, timeout/cancellation and dropping the run at tool handoff. No live state survives to a second HTTP request.
- Unary `GetUsableModels` uses `application/proto` and a **bare** protobuf request/response (not a five-byte Connect envelope). IDs and server-reported max flags remain exact; aliases, suffix inference and fallback routes are intentionally absent. Optional AvailableModels supplies declared parameter routes; GetDefaultModelForCli is metadata only.
- Model route carries the separate legacy model ID, requested ID, parameter list and optional max mode; default exact routing invents no parameters.
- The Connect decoder checks split/sticky framing, flag bits, gzip checksum/trailing bytes, JSON error end envelopes and status trailers. Bounds are 4 MiB compressed, 8 MiB inflated/uncompressed, and 32 MiB total content-addressed blob data.
- Successful completion requires `turnEnded`, clean Connect-end or explicit status-0 trailers, complete framing, and HTTP EOF after trailers. Bare EOF, `stepCompleted`, unknown operation or a truncated frame cannot imply success. A late trailer error wins over `turnEnded`.
- The first caller-owned MCP tool produces IR ToolCallStart/Delta/Complete and Done(tool_use), then **local ToolHandoff**, not a fabricated upstream completion. Argument snapshots are buffered and checked against final Value-encoded arguments; the final argument delta is emitted only once verified. Transport closes the run immediately and must not call `finish()` after this local terminal.
- No native tools are executed. Shell/write/delete/read/list have typed rejection responses; grep has its typed error response. Other known exec families receive typed ExecClientThrow + stream close and an explicit local Unsupported failure. Unknown operations and approval queries also fail. The transport must send rejection actions preceding Action::Failure before surfacing the failure.
- `rootPromptMessagesJson` is rebuilt from complete caller history; typed turns contain SHA-256 references to user/assistant/thinking/MCP steps and paired results. Empty content/results and error results survive. Orphan, duplicate or unanswered tool calls are rejected rather than converted to fabricated assistant text or XML.
- Caller system configuration is sent as clearly labelled global USER rules in RequestContext as well as historical system blobs. This is best effort under Cursor's server-owned prompt; it is not an allowlisted `custom_system_prompt` override and never inserts an assistant acknowledgement.
- MCP schemas and each argument value are serialized `google.protobuf.Value`, **not** Struct or JSON byte strings. Non-object arguments, malformed Values, missing kinds, non-finite numbers and inexact large integers fail. Schemas are forwarded; this module does not claim full JSON Schema enforcement.
- Tool names are preserved, including names that happen to resemble native tools. Caller tools are always external MCP tools. Undeclared tools fail. `none` advertises no tools; named choice restricts the advertised set; required/named choices fail if a normal terminal arrives without a tool handoff.
- Positive `max_tokens` is accepted for compatibility with clients requiring it, but is **not enforced**; integration must expose `token_budget_not_enforced`. Other unmapped generation controls, structured output, multimodal input, custom/namespaced tools, and interactive/native workflows fail explicitly. Reasoning is resolved only from exact entitled variants in the same declared parameter/cost lane; unmapped controls, explicit budgets and display controls fail.
- Only authoritative TurnEnded input/output counters are projected into integer-valued IR Usage. The session's `CursorUsage` keeps absent counters as `None`; tokenDelta estimates and checkpoint occupancy are never used to guess completion usage.

## Verification scope

Inline offline tests cover frame splits/sticky/gzip/limits/trailers/truncation,
exact catalog IDs, full-history reconstruction, empty/error tool results,
Value schema/argument encoding, blob integrity/isolation, context handshake,
text/thinking, verified terminals, tool handoff, native rejection and typed
unsupported/malformed failures. No real credential or upstream smoke test is
implied by these tests. The integrating parent runs the workspace Cargo checks.

## Third-party MIT notice

MIT License

Copyright (c) 2025 Mario Zechner
Copyright (c) 2025-2026 Can Bölük
Copyright (c) 2026 Stencil Labs, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
