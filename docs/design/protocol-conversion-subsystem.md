# Unified Protocol Conversion Subsystem

- **Status:** Proposed implementation plan; architectural direction accepted
- **Date:** 2026-08-22
- **Branch:** refactor/unified-protocol-conversion
- **Baseline commit:** 87bd92bf1c76
- **ADR:** [ADR 0001](../adr/0001-unified-protocol-conversion-strategies.md)

## Implementation progress

- **Stage 0 complete:** parity audit restored to COMPLETE (1168 inventory entries, pending 0), mapped assertions strengthened, PassThrough/Compat behavior characterized, and current strategy diagnostics added.
- **Stage 1 complete:** the top-level `conversion` module now defines closed, owned `ConversionPlan` variants, independent request/response leg modes, capability declarations, invariant-enforcing constructors, and stable diagnostic names. Runtime dispatch behavior is unchanged.
- **Stage 2 complete:** raw-wire eligibility, profile/session selection, rule IDs, and IR-to-wire patch generation now live in `conversion::resolver` / `conversion::wire_patch`; dispatcher compatibility wrappers delegate to the subsystem. Legacy and new selectors were shadow-compared across the complete 36-test Compat dispatcher suite before the duplicate implementation was removed.
- **Stage 3 complete:** every provider attempt now produces one owned `ResolvedConversion`, pairing the public `ConversionPlan` with optional private Raw-Wire state. PassThrough, mixed Native IR legs, and Raw-Wire Compat all drive the same strategy diagnostics while existing execution handlers remain unchanged.
- **Stage 4 complete:** `PreparedConversion`, `PreparedBody`, and `PreparedSession` now carry all three strategies into execution. Raw-Wire preparation, hook/vendor wire patch replay, and exact-body validation moved out of dispatcher glue; existing Raw-Wire, stream, and buffered handlers are reconstructed through a narrow adapter without changing their behavior.
- **Stage 5 complete:** every strategy now returns one `ConversionAttempt` with `RetryDisposition` and `HealthDisposition`. The dispatcher no longer combines a Compat-only force-retry boolean with a separate native tuple; Raw-Wire forced retries and the shared HTTP status policy flow through one boundary.
- **Stage 6 complete (safe semantic seam):** native and Raw-Wire buffered success paths now share one `finalize_buffered_response` lifecycle for model normalization, legacy hooks, `OnResponse`, usage capture, and mutation detection. Raw-Wire retains exact client bytes when no hook mutates semantics; transport, decoding, error envelopes, and logging remain strategy-owned. Streaming code was not changed.
- **Stage 7 complete (safe streaming seam):** the per-delta `OnResponse` streaming hook lifecycle is now shared. `dispatcher/streaming.rs` owns `StreamHookState`: capture-once owned context cloning (zero-clone when no hooks register) and one `apply()` used identically by native IR streaming and Raw-Wire compat streaming. The duplicated `run_stream_on_response` / `apply_stream_hooks` implementations and their clone preambles were deleted. Compat SSE and native `AiStreamDelta` state machines, cancellation/deadline select loops, priming, terminal observation, health settlement, and logging remain strategy-owned and unchanged.
- **Stage 8 cleanup (glue removal):** the dispatcher's `supports_compat_request` / `select_compat_request` / `CompatRequest` compatibility wrappers are deleted; `dispatch_pipeline_inner` calls `conversion::supports_raw_wire_compat` / `conversion::resolve_raw_wire_compat` directly. Compat tests exercise the resolver through the same public conversion API. Remaining in `dispatcher/compat.rs` are only strategy-owned execution: header normalization, transport, error envelopes, priming, streaming SSE state machine, and its buffered finalize adapter.

## 1. Executive summary

Nyro will expose one Protocol Conversion subsystem with three explicit strategies:

~~~text
Protocol Negotiation
        |
        v
Per-target Conversion Planning
        |
        +-- PassThrough
        +-- Native IR
        +-- Raw-Wire Compat
        |
        v
Shared attempt lifecycle
transport -> buffering/streaming -> hooks -> usage -> retry/health -> logging
~~~

The refactor unifies capability boundaries, planning, outcomes, and policy-neutral execution. It does not merge the semantic engines or force all data through one representation.

## 2. Why this refactor exists

Today the proxy dispatcher is the accidental conversion facade. It directly coordinates:

- compatibility eligibility and profile selection;
- request and response pass-through flags;
- native tool routing and parameter overrides;
- IR-to-wire patch generation for compatibility requests;
- provider request building and header precedence;
- buffered versus streaming handlers;
- compatibility-specific retry hints;
- provider health settlement;
- logging and usage extraction.

The current integration seam is large and growing:

- <code>proxy/dispatcher/mod.rs</code>: 2,233 lines at the design baseline;
- <code>proxy/dispatcher/compat.rs</code>: 3,025 lines at the design baseline;
- the original porting report recorded the compat integration file at 2,605 lines.

The recent Responses-Lite carrier defect also demonstrated path drift: one tool-carrier behavior affected Responses-to-Responses, Responses-to-Chat, and Responses-to-Anthropic paths.

## 3. Goals

1. Make strategy selection one explicit, testable per-target decision.
2. Give the dispatcher a strategy-neutral plan and attempt outcome.
3. Preserve Protocol Negotiation as a pure deterministic concern.
4. Preserve Raw-Wire Compat's bytes, ordered JSON, sessions, pair transforms, and parity audit.
5. Preserve Native IR's general semantic codec model and hook integration.
6. Make PassThrough a first-class strategy with an honest contract.
7. Consolidate transport, cancellation, deadlines, logging, usage, retry, and health where semantics are actually common.
8. Keep every migration slice compiling, testable, reversible, and behavior-preserving.
9. Improve observability so every request explains which strategy and rule were selected.

## 4. Non-goals

- Rewriting <code>nyro-ccswitch-compat/src/ported</code> into idiomatic Nyro code.
- Routing Raw-Wire Compat through <code>AiRequest</code>, <code>AiResponse</code>, or <code>AiStreamDelta</code> as its source of truth.
- Removing the native IR or replacing it with pairwise transforms.
- Adding a new external protocol or provider as part of the refactor.
- Changing database schemas or adding WebUI configuration.
- Promising byte-identical request forwarding for PassThrough.
- Building a public runtime plugin API for conversion strategies.
- Starting with streaming consolidation.

## 5. Canonical terminology and contracts

### 5.1 ProtocolPlan

The existing pure negotiation result. It answers which upstream endpoint and protocol are selected. It must not depend on raw request bytes, provider implementation quirks, or hook mutation results.

### 5.2 ConversionPlan

An owned per-attempt plan resolved after:

- a target provider is selected;
- the effective base URL and actual model are known;
- OnUpstream hooks have run;
- the raw envelope and current IR are available.

A retry against another target resolves a fresh ConversionPlan and a fresh prepared session.

### 5.3 PassThrough

PassThrough means no cross-protocol semantic translation. Current request behavior still parses JSON and may apply narrow changes such as the actual model, usage options, developer-role normalization, provider sanitization, authentication, and URL construction. Response pass-through may forward upstream bytes directly when response mutation is not declared.

Therefore request and response leg modes must remain independent. Exact request-byte forwarding is a possible future optimization, not an invariant of this refactor.

### 5.4 Native IR

Native IR decodes and encodes normalized protocol meaning through existing endpoint handlers. It owns semantic repair, tool routing, reasoning normalization, semantic hooks, and general cross-protocol conversion.

### 5.5 Raw-Wire Compat

Raw-Wire Compat is a paired, stateful request/response conversion. It owns compatibility profile selection, session identity, private carrier restoration, pair-specific streaming state, client error envelopes, and semantic-failure detection. It requires exact raw request bytes.

## 6. Target architecture

### 6.1 Two-stage planning

~~~text
Ingress decoder produces AiRequest + RawEnvelope
        |
        v
Protocol Negotiation: ProtocolPlan
        |  ingress/egress/endpoint/auth/base URL facts
        v
Target selected + OnUpstream hooks
        |
        v
Conversion Resolver: ConversionPlan
        |  provider/vendor/channel/model/raw body/mutation facts
        v
Strategy preparation
        |
        v
Shared attempt executor
~~~

Conversion resolution must stay inside the target loop. Same endpoint pairs can require different strategies for xAI, OpenAI/Codex, third-party strict Responses, or DeepSeek/MiMo normalization.

### 6.2 Strategy precedence

1. **Required Raw-Wire Compat** when a narrow compatibility rule matches.
2. Otherwise compute native mutation requirements.
3. **PassThrough** when both legs can skip IR conversion under current safety rules.
4. **Native IR** when either leg requires semantic encoding/decoding.

Same-protocol does not imply PassThrough. Same-protocol Raw-Wire Compat normalization is valid.

### 6.3 Request and response leg modes

A single request can currently use native request preparation while passing response bytes through, or bypass IR on the request while requiring response decoding. The plan therefore preserves per-leg modes.

Conceptually:

~~~rust
pub enum ConversionKind {
    PassThrough,
    NativeIr,
    RawWireCompat,
}

pub enum ConversionPlan {
    PassThrough(PassThroughPlan),
    NativeIr(NativeIrPlan),
    RawWireCompat(WireCompatPlan),
}

pub struct NativeIrPlan {
    pub request_mode: NativeRequestMode,
    pub response_mode: NativeResponseMode,
}

pub enum NativeRequestMode {
    PassThroughJson,
    EncodeIr,
}

pub enum NativeResponseMode {
    PassThroughBytes,
    DecodeEncodeIr,
}
~~~

Raw-Wire Compat remains a paired plan because its response conversion depends on request-derived session state.

## 7. Proposed module layout

Target layout:

~~~text
crates/nyro-core/src/conversion/
├── mod.rs
├── plan.rs                 # ConversionKind, plans, capabilities
├── resolver.rs             # per-target deterministic selection
├── prepared.rs             # prepared bodies and owned sessions
├── outcome.rs              # retry, health hint, usage, metadata
├── service.rs              # later: process-scoped CompatEngine ownership
├── strategy/
│   ├── mod.rs
│   ├── pass_through.rs
│   ├── native_ir.rs
│   └── raw_wire_compat.rs
└── execution/
    ├── mod.rs
    ├── buffered.rs
    ├── streaming.rs
    ├── hooks.rs
    └── transport.rs
~~~

The first slices add only plans, resolution, and adapters. Existing dispatcher handlers remain in place until the contracts stabilize. Execution modules are populated later as code is extracted.

Ownership remains:

- <code>protocol/codec</code> and <code>protocol/ir</code>: semantic codecs and IR;
- <code>nyro-ccswitch-compat</code>: byte-level compatibility engine and pinned port;
- <code>provider</code>: URL, auth, and vendor-owned request policy;
- <code>conversion</code>: strategy planning, preparation, conversion outcomes, and eventually shared execution;
- <code>proxy/dispatcher</code>: route/auth/target retry orchestration, consuming ConversionPlan.

## 8. Resolution API

The implemented resolver uses two deterministic internal steps rather than an async trait or dynamic registry. Raw-Wire eligibility must be known before native tool routing, while final PassThrough/IR leg modes are only known after parameter and vendor mutation facts are computed.

~~~rust
pub(crate) struct ResolveRawWireCompatInput<'a> {
    ingress: ProtocolId,
    egress: ProtocolId,
    provider: &'a Provider,
    egress_base_url: &'a str,
    actual_model: &'a str,
    client_stream: bool,
    headers: &'a HeaderMap,
    raw_body: &'a [u8],
    baseline_request: &'a AiRequest,
    current_request: &'a AiRequest,
}

pub(crate) fn resolve_raw_wire_compat(
    input: ResolveRawWireCompatInput<'_>,
) -> Result<Option<RawWireCompatSelection>, String>;

pub(crate) struct ResolveConversionInput {
    ingress: ProtocolId,
    egress: ProtocolId,
    raw_wire: Option<RawWireCompatSelection>,
    protocol_is_native: bool,
    request_passthrough: bool,
    response_passthrough: bool,
}

pub(crate) fn resolve_conversion(
    input: ResolveConversionInput,
) -> Result<ResolvedConversion, ConversionPlanError>;
~~~

<code>ResolvedConversion</code> owns the final public <code>ConversionPlan</code> plus optional strategy-private Raw-Wire profile/session/patch state. The dispatcher can inspect the plan for diagnostics and leg modes without exposing ordered JSON or cc-switch implementation types as the public planning contract.

RawEnvelope's flattened header map is not sufficient for compatibility identity and repeated-header behavior. The resolver therefore retains the exact HTTP HeaderMap input until RawEnvelope is separately evolved to preserve it losslessly.

## 9. Preparation API

Resolution is deterministic; preparation is async only for the Raw-Wire strategy because it creates request-derived cc-switch session state.

~~~rust
pub(crate) enum PreparedBody {
    Json(serde_json::Value),
    Raw(bytes::Bytes),
}

pub(crate) enum PreparedSession {
    PassThrough,
    NativeIr { ingress: ProtocolId, egress: ProtocolId },
    RawWireCompat(Box<nyro_ccswitch_compat::ConversionSession>),
}

pub(crate) struct PreparedConversion {
    plan: ConversionPlan,
    body: PreparedBody,
    force_upstream_stream: bool,
    session: PreparedSession,
}

pub(crate) async fn prepare_conversion(
    input: PrepareConversionInput<'_>,
) -> Result<PreparedConversion, PrepareConversionError>;
~~~

The adapter keeps existing <code>OutboundRequest</code> URL/header ownership in the provider layer. Native and PassThrough bodies remain JSON; Raw-Wire remains exact Bytes. The Raw-Wire branch performs client IR patch replay, vendor before/after patch replay, and typed missing-raw-body validation before reconstructing the existing <code>PreparedRequest</code> for the unchanged handler.

Plans and prepared sessions contain no borrowed Provider, dispatcher CallCtx, endpoint handler, or raw-body references. Every target retry gets a fresh owned prepared session, ready for later movement into spawned stream tasks.

## 10. Strategy responsibilities

| Responsibility | PassThrough | Native IR | Raw-Wire Compat |
|---|---|---|---|
| Cross-protocol semantics | None | General IR conversion | Pair-specific conversion |
| Request source of truth | Parsed native JSON under existing rules | AiRequest | Exact raw Bytes + IR patch |
| Response source of truth | Upstream bytes when safe | AiResponse/AiStreamDelta | Converted wire Bytes/SSE |
| Provider URL/auth | Provider layer | Provider layer | Provider layer + compat header delta |
| Stateful request/response inversion | No | Per-request codec state | ConversionSession + CompatState |
| Semantic hooks | Existing native behavior | Full | Current decode/re-encode or patch behavior |
| Unknown field policy | Preserve under native JSON rules | IR/extension policy | Direction-specific whitelist/preserve rules |
| Parity authority | Pass-through tests | Native conversion matrix | cc-switch parity inventory |

## 11. Strategy-neutral outcomes

### 11.1 Transitional attempt outcome

The first common outcome can still carry an Axum Response to minimize churn:

~~~rust
pub struct ConversionAttempt {
    pub response: axum::response::Response,
    pub retry: RetryDisposition,
    pub health: HealthDisposition,
}

pub enum RetryDisposition {
    DefaultStatusPolicy,
    ForceRetry,
}

pub enum HealthDisposition {
    Success,
    Failure,
    Neutral,
    Deferred,
}
~~~

This replaces the current combination of compatibility force-retry plus a separate status-policy check.

### 11.2 Long-term converted output

After buffered and streaming execution are extracted, conversion code should return strategy-neutral metadata and bodies rather than constructing Axum responses directly:

~~~text
ConvertedOutput
- status and response headers
- buffered bytes or owned client stream
- canonical Usage
- optional semantic response view
- retry disposition
- health hint / stream commit state
- diagnostics and selected rule ID
~~~

The dispatcher remains responsible for target retry iteration and final route selection.

## 12. Hooks and mutation capability

Hook behavior cannot be assumed identical across strategies.

Define explicit capabilities:

- request semantic mutation supported;
- request wire patch supported;
- buffered semantic response mutation supported;
- stream semantic delta mutation supported;
- opaque wire observation only.

Initial migration preserves existing behavior exactly:

- Native IR hooks operate directly on semantic values.
- Raw-Wire Compat request hooks are replayed through the existing baseline/current wire patch.
- Raw-Wire Compat buffered/stream responses retain the current opportunistic decode and re-encode behavior when a hook actually changes semantic output.
- PassThrough keeps current response observation and usage side parsing.

Do not introduce new runtime rejections for hook/strategy combinations in a structural slice. Capability enforcement is a later, separately reviewed behavior change.

## 13. Streaming design

Streaming is the last migration stage because it combines:

- stateful parser/formatter ownership;
- Send + static requirements for spawned tasks;
- first-chunk priming and retry-before-commit;
- client cancellation and deadlines;
- terminal-event semantics;
- provider health settlement after stream completion;
- usage and logging after the response has already been returned.

Rules:

1. Prepared stream sessions are owned and Send; Sync is not required.
2. No borrowed CallCtx, ProviderCtx, or endpoint handler crosses into a spawned task.
3. A target retry creates a new prepared session.
4. Compat pair-specific SSE state machines remain untouched.
5. Native AiStreamDelta state machines remain untouched.
6. The common driver owns transport, cancellation, deadline, metrics, and lifecycle settlement.
7. A strategy callback owns semantic failure detection, event conversion, and client-specific terminal/error output.

## 14. Error, retry, health, and commit model

The common taxonomy must distinguish:

- client-invalid conversion input: never retry another provider;
- provider/transport failure: status-policy or forced retry;
- semantic failure inside a successful HTTP status;
- failure before stream response commitment: retry remains possible;
- failure after response commitment: no provider retry;
- client cancellation: health neutral unless an independent provider error occurred;
- complete, incomplete, truncated, and timed-out streams.

CompatError is mapped once at the Raw-Wire adapter boundary. GatewayError is mapped at the Native IR boundary. Strategy-specific client error envelopes remain strategy-owned.

## 15. Observability

Add strategy facts to RequestContext trace and tracing spans before changing persisted log schemas:

- <code>conversion.strategy</code>: pass_through, native_ir, raw_wire_compat;
- <code>conversion.rule</code>: stable selection rule ID;
- <code>conversion.request_mode</code>;
- <code>conversion.response_mode</code>;
- <code>conversion.force_upstream_stream</code>;
- <code>conversion.retry_disposition</code>;
- <code>conversion.commit_state</code> for streams.

This lets tests and production diagnostics answer why a path was selected without a database migration. Any future persisted fields require the repository's schema-update process and are outside the initial refactor.

## 16. Migration plan

### Stage 0 — restore the behavior baseline

Before moving parity-sensitive code:

1. Repair the current cc-switch parity audit. The branch baseline initially reported Integrity OK but Completeness INCOMPLETE with 16 pending mappings; Stage 0 restored COMPLETE with 0 pending while isolating Nyro reasoning extensions outside the mechanically ported transforms.
2. Update the stale completeness claims in the porting report.
3. Add table-driven tests for every positive and negative compatibility selection rule.
4. Add current PassThrough request/response-leg characterization tests.
5. Add conversion strategy diagnostics without changing execution.

### Stage 1 — introduce types only

- Add ConversionKind, ConversionPlan, per-leg modes, capabilities, and typed resolver errors.
- Add construction and invariant tests.
- No dispatcher behavior changes.

### Stage 2 — extract the resolver

- Move compatibility supports/select logic behind resolve_conversion.
- Keep temporary wrapper functions in dispatcher/compat.rs.
- Shadow-run old and new pure selection in tests/debug builds and assert exact equivalence.
- Do not send duplicate upstream traffic.

### Stage 3 — use one plan per target

- Replace compat_candidate, passthrough_req, and passthrough_resp calculations with ConversionPlan.
- Continue delegating to existing prepare and handler functions.
- Preserve independent request/response leg modes.

### Stage 4 — introduce prepared sessions

- Wrap CompatEngine preparation in Raw-Wire strategy adapter.
- Add PassThrough and Native IR prepared variants.
- Keep existing OutboundRequest representation initially.
- Require exact raw_body for Raw-Wire Compat.

### Stage 5 — unify attempt outcomes

- Replace compatibility-specific force-retry and dispatcher health tuples with RetryDisposition and HealthDisposition.
- Normalize Usage and response metadata at one boundary.
- Preserve client-specific envelopes.

### Stage 6 — consolidate buffered execution

- Extract common upstream buffered call, bounded body read, decompression, metadata, logging, usage, hooks, retry/health settlement, and response construction.
- Strategy adapters retain response conversion and semantic-failure interpretation.

### Stage 7 — consolidate streaming execution

- Introduce owned prepared stream sessions.
- Extract the common stream driver, cancellation/deadline handling, priming protocol, metrics, logging, and deferred health settlement.
- Keep Compat and Native stream conversion state machines separate.

### Stage 8 — cleanup ownership and old glue

- Introduce a ConversionService that owns the process-scoped CompatEngine if useful.
- Remove protocol-pair matrices and concrete Compat types from dispatcher.
- Delete obsolete branches and wrappers only after all gates pass.
- Update architecture and porting documentation.

## 17. Verification gates

Every stage must pass the narrow relevant tests plus the following accumulated gates:

### Structural gates

- cargo fmt --all -- --check
- cargo check -p nyro-core
- cargo clippy -p nyro-core --all-targets
- cargo test -p nyro-core

### Raw-Wire gates

- cargo test -p nyro-ccswitch-compat --lib
- parity inventory with --require-complete
- raw request and buffered response goldens for every compat direction
- ordered JSON and private-carrier tests
- session identity and cross-request state tests

### Streaming gates

- arbitrary chunk-boundary and UTF-8 split tests
- LF/CRLF and unterminated-final-block tests
- parallel tool calls and late usage
- first-chunk semantic error and retry-before-commit
- incomplete, truncated, error, timeout, and cancellation paths
- no duplicate terminal events

### Cross-strategy contract gates

- strategy-selection table including negative cases
- request/response mixed leg-mode tests
- hook observation and mutation parity
- canonical usage parity
- retry and health outcome parity
- same-protocol PassThrough characterization
- cross-protocol conversion matrix and Python E2E proxy matrix

### Performance gates

Capture a baseline before execution extraction. Reject material regressions in:

- time to first byte;
- allocations for native same-protocol traffic;
- streaming throughput;
- memory retained for long responses;
- compatibility first-chunk priming.

## 18. Rollback and safety

- One conceptual change per commit/slice.
- Old wrappers remain until the replacement passes equivalence tests.
- Shadow only pure planning decisions; never execute a non-idempotent upstream request twice.
- Keep the current handlers callable until their corresponding new executor is verified.
- Do not combine structural movement with protocol behavior fixes.
- A failed stage is reverted without depending on later stages.
- Do not deploy this branch to production until the full verification gate passes; production operations remain user-controlled.

## 19. Principal risks and mitigations

| Risk | Mitigation |
|---|---|
| Strategy selection moves to the wrong lifecycle phase | Resolve per target after OnUpstream hooks |
| Same-protocol normalization mistaken for PassThrough | Raw-Wire rules have higher priority |
| Request/response mixed modes are lost | Model leg modes independently |
| Compat raw body silently reconstructed | Typed MissingRawBody failure |
| Ordered JSON leaks into nyro-core | Keep byte-only Compat boundary |
| Session reused across retry targets | Prepare a fresh owned session per attempt |
| Async trait/object-safety complexity | Start with closed enums and free functions |
| Streaming refactor changes commitment timing | Migrate streaming last with first-chunk tests |
| Hook support regresses silently | Explicit capability model and parity tests |
| Refactor changes provider behavior | Separate structural and behavioral commits |

## 20. Definition of done

The refactor is complete when:

1. ProtocolPlan remains pure and unchanged in responsibility.
2. Every provider attempt resolves one explicit ConversionPlan.
3. The dispatcher no longer owns a protocol-pair compatibility matrix.
4. PassThrough, Native IR, and Raw-Wire Compat are visible strategy choices.
5. Request and response leg modes are explicit.
6. Retry, health, usage, and conversion diagnostics use one common outcome model.
7. Buffered and streaming lifecycle mechanics are shared where policy-neutral.
8. Native and Compat semantic/stateful converters remain independent.
9. The compatibility crate's ordered JSON and ported source boundaries are intact.
10. The parity inventory is COMPLETE and all conversion/E2E gates pass.
11. No production-observable behavior changes without a separately documented decision.
12. Architecture, glossary, and maintenance documentation describe the final subsystem.
