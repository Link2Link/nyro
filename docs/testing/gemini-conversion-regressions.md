# Native Gemini conversion repairs and regression evidence

[简体中文](gemini-conversion-regressions_CN.md)

## Status

The four approved repairs are implemented. The original test-only baseline was
`14a27aa` (`nyro-core` 2.0.9): 21 new tests exposed 13 failed assertions and eight
passing controls. Those tests were not ignored or weakened; the corrected
implementation also has additional boundary and local-dispatcher tests.

No provider settings, dependencies, database schema, deployed service, or release
version were changed. No commit was created. Synthetic fixtures and loopback
upstreams are not evidence of acceptance by a live Google/Vertex account.

## Implemented behavior

### A. Request-local tool identity

`google/gemini/stream.rs` now owns a per-parser call counter and explicit-ID
snapshot state:

- Independent complete calls, including same-name and identical no-ID calls,
  receive distinct indices. Start and argument Delta use the same index.
- SSE transport boundaries do not reset indices; independent requests share no
  state and need no global cleanup map.
- Nonempty upstream IDs are preserved. Missing IDs are synthesized.
- Exact complete replays with the same explicit ID do not emit duplicate calls or
  concatenate arguments. Conflicting payload/name for the same ID is a terminal
  stream error, not another executable call.
- The parser does not guess cumulative semantics from text prefixes, names, or
  part positions. General partial-argument/cumulative dialects remain outside the
  supported complete-call contract.

This fixes OpenAI Chat wire index collisions, forced-stream accumulation losing
all but the final call, and Responses custom-tool bridge buffers mixing inputs.
The accumulator and each formatter retain their existing index contract.

### B. Explicit zero arguments and safe error termination

A complete `functionCall` with `args: {}` emits the argument delta `"{}"` exactly
once. It now survives Gemini re-encoding. Missing/nonobject arguments, invalid
function names, and malformed/truncated JSON do not synthesize executable calls.

A shared dispatcher batch guard recognizes both decoder `Err` and IR
`StreamError`/`UnexpectedEof` **before** buffering tools or invoking formatters.
On failure:

- Native streaming emits the existing protocol-specific error event and skips
  successful completion and pending custom-tool flushing.
- Forced-upstream-stream/nonstream handling returns HTTP 502 rather than a partial
  successful tool response.
- Failure is recorded; the original upstream HTTP status can still be 200.

Already delivered content from earlier valid stream batches cannot be revoked.
This is not a new retry mechanism, nor a guarantee that arbitrary third-party
stream formats are supported.

### C. Function names resolved from call identities

`google/gemini/tool_names.rs` performs encoding-local lookup for text results and
ToolResult blocks. It keeps IR IDs separate from outgoing function names, includes
ToolUse blocks, tolerates duplicate representations of one call, and rejects
conflicting active identities. Tests use distinct function names with results in
reverse order, so FIFO-only guessing cannot pass.

Call/result wire IDs are preserved when available, including native Google input.
Legacy Google results containing only a name are paired against pending same-name
calls in occurrence order; opaque OpenAI/Anthropic IDs do not use this fallback.
The outgoing names are those after namespace/custom-tool preparation.

Unresolvable results are typed bad requests. Generic orphan repair is tagged with
internal provenance so a synthetic `unknown_tool` is not mistaken for a recovered
real function name. That provenance is not forwarded to other protocol wires.
Existing tests that asserted `functionResponse.name == call_id` now assert the
actual name and paired ID instead; orphan fixtures use explicit policy tests.

### D. Bounded semantic schema lowering

`google/gemini/schema.rs` lowers the restricted native `parameters` subtree before
schema-aware cleanup:

- Local JSON Pointer `$ref`/legacy `ref` expansion against the original root,
  including escaped tokens and URI fragments. A path-local recursion stack
  distinguishes real cycles from repeated sibling references.
- Compatible object `allOf` merges with first-appearance required ordering;
  conflicting or unproven closed-object intersections are rejected, not overwritten.
- Exact distinct string singleton alternatives become an enum. Arbitrary unions,
  duplicate `oneOf` alternatives, and lossy intersections are not guessed.
- Required and nullable remain independent. Property names and literal
  `default`/`enum` payloads are not interpreted as schema keywords.
- Lowering is bounded to depth 64 and 10,000 visited/copied nodes, inclusive.
  Tests cover exact boundaries and repeated-reference expansion. External refs
  are never fetched.

Unrepresentable native structures return a typed `ProtocolLossyRejected` (422)
instead of an empty success schema or an accidental internal error. The pipeline
preserves typed codec errors without reclassifying unrelated `anyhow` failures.
Existing explicitly selected `parametersJsonSchema` wire content is preserved;
there is no unverified automatic endpoint-wide switch to the richer channel.

## Native versus compat validation boundary

The dispatcher marks a private, non-client-deserializable
`RequestMetadata.raw_wire_preview` only after selecting raw-wire compat. Native
before/after encoding on that path is vendor-diff material, not the final request;
its schema/orphan validation cannot veto a body the selected compat engine can
represent. The compat engine still constructs/validates the actual wire body.

Exposure is route-specific:

- Google `antigravity`/`gemini-cli` subscription channels explicitly use native IR
  to execute envelope hooks and forced-upstream-stream handling.
- Chat/Responses client requests to Gemini egress use the native converter.
- Ordinary Anthropic-to-direct-Gemini generally selects cc-switch, whose richer
  schema/name behavior remains intact.
- Unmodified same-protocol passthrough preserves original wire fields.
- Vertex AI has both native Gemini and OpenAI-compatible channels; impact follows
  the negotiated endpoint, not a provider label.

## Verification results

The final repository gate completed with **zero failing targets**:

- Native stream regressions: **21 passed**.
- Native request regressions: **16 passed**.
- Loopback dispatcher route regressions: **6 passed**.
- `nyro-core` library unit tests: **725 passed**, including the actual accumulator,
  bounded schema lowering, preview ownership, and stream-error propagation tests.
- Complete workspace gate (`--exclude nyro-desktop --no-default-features
  --no-fail-fast`): **passed**; existing server/doc-test ignores remain unchanged.
- `cargo clippy -p nyro-core --all-targets`: **passed with existing repository
  warnings**; it was not a `-D warnings` run.
- `cargo fmt --all -- --check` and `git diff --check`: **passed**.

## Test layout and commands

| Location | Purpose |
| --- | --- |
| `crates/nyro-core/tests/conv_google_stream_regressions.rs` | Identity, request isolation, wire reconstruction, custom bridge, zero args, malformed input, explicit IDs and transport splits |
| `crates/nyro-core/tests/conv_google_request_regressions.rs` | Reversed-result names, ID round trips, legacy pairing, orphan rejection, refs/compositions, nullable/required and other-protocol controls |
| `crates/nyro-core/src/proxy/dispatcher/accumulator.rs` `#[cfg(test)]` | Real private forced-stream accumulator; no copied implementation or visibility workaround |
| `crates/nyro-core/src/protocol/codec/google/gemini/schema.rs` tests | Local refs, escapes, cycles, exact enum/intersection rules and resource limits |
| `provider/common/pipeline.rs` tests | Typed error status and compat-owned preview validation |
| `proxy/dispatcher/streaming.rs` / `non_stream.rs` tests | Atomic decoder-error batches and forced-stream 502 behavior |
| `crates/nyro-core/tests/gemini_route_regressions.rs` | Real dispatcher plus loopback upstream: native, compat, passthrough, Vertex, rejected schema and failed streaming |

Targeted runs:

```bash
cargo test -p nyro-core --test conv_google_stream_regressions
cargo test -p nyro-core --test conv_google_request_regressions
cargo test -p nyro-core --test gemini_route_regressions
cargo test -p nyro-core --lib proxy::dispatcher::accumulator::tests
cargo test -p nyro-core --lib proxy::dispatcher::streaming::tests
cargo test -p nyro-core --lib proxy::dispatcher::non_stream::tests
cargo test -p nyro-core --lib protocol::codec::google::gemini::
```

Existing conversion and repository gates:

```bash
cargo test -p nyro-core --test conv_google --test conv_cross_provider \
  --test conv_streaming --test conv_tool_pairing --test protocol_conversion
cargo test --workspace --exclude nyro-desktop --no-default-features --no-fail-fast
cargo clippy -p nyro-core --all-targets
git diff --check
```

The local route harness explicitly prevents Gateway startup refresh/OAuth monitor
tasks from being polled. It uses memory storage, inert tokens, no proxy/redirects,
and loopback HTTP only. Subscription runtime authentication pins real service
hosts, so no live subscription inference is performed: that path is covered by
pipeline/forced-stream handler tests rather than a fake production credential.

## Deliberately not changed

- No universal adjacent-role merge or claim that consecutive users cause empty
  HTTP 200 responses; that causal claim was not established.
- No general cumulative-snapshot guessing or global OpenAI missing-index rewrite.
- No rule that nullable means optional, or that OpenAI strict forbids local refs.
- No quota cooldown, image mapping, model catalog, retry, or unrelated signature/
  response-metadata redesign.
- No claim of live upstream verification or deployment. Account/model-specific
  smoke remains a separate authorized operation.
