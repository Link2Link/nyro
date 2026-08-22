# ADR 0001: Unify Protocol Conversion Behind Three Strategies

- **Status:** Accepted
- **Date:** 2026-08-22
- **Branch:** refactor/unified-protocol-conversion
- **Baseline:** 87bd92bf1c76
- **Detailed design:** [Protocol Conversion subsystem](../design/protocol-conversion-subsystem.md)

## Context

Nyro currently exposes one product capability through three implicit execution paths:

1. same-protocol pass-through behavior;
2. semantic conversion through the native Intermediate Representation;
3. pair-specific raw-wire conversion through the cc-switch compatibility engine.

The paths are selected and orchestrated by conditionals in the proxy dispatcher. Request preparation, stream handling, hooks, usage extraction, error mapping, retry classification, health accounting, logging, and response construction are partly duplicated. The integration hotspot has continued to grow, while the native and compatibility engines intentionally provide different guarantees.

The native IR represents normalized protocol meaning and supports composable N+M codec conversion. Raw-Wire Compat preserves pair-specific behavior that is not safely reducible to semantic IR: ordered JSON, private carriers, session state, headers, SSE event ordering, first-chunk commitment, error envelopes, and raw-wire parity with the pinned cc-switch source.

PassThrough is also a real strategy. It skips semantic transcoding, but the request side may still apply narrow provider/model/protocol defaults; it is not defined as byte-identical forwarding.

## Decision

Nyro will present Protocol Conversion as one subsystem with three explicit strategies:

- **PassThrough** — same-protocol conversion without semantic transcoding;
- **Native IR** — general semantic conversion through Nyro IR;
- **Raw-Wire Compat** — pair-specific cc-switch-compatible wire conversion.

Protocol Negotiation and Conversion Planning remain separate decisions:

- <code>ProtocolPlan</code> stays pure and selects the upstream protocol endpoint.
- A per-target <code>ConversionPlan</code> is resolved after provider selection and OnUpstream hooks, when provider, channel, actual model, base URL, raw request, stream mode, and mutation requirements are known.

The initial implementation will use closed enums and free functions rather than a dynamic strategy registry. Plans and prepared sessions are owned values. Raw-Wire Compat keeps its ordered JSON implementation, state machines, and session types private behind a byte-oriented adapter.

Common orchestration will be consolidated incrementally: planning, attempt outcomes, transport, buffered handling, then streaming. Pair-specific transforms and stream state machines remain strategy-owned.

## Invariants

1. The mechanically ported cc-switch source remains in its independent crate and is not rewritten into native codecs.
2. The ordered JSON implementation does not cross the compatibility crate boundary.
3. Raw-Wire Compat never reconstructs a missing raw request body from parsed JSON.
4. Strategy resolution happens for each provider attempt; prepared sessions are not reused across different targets.
5. Protocol Negotiation remains deterministic and provider-capability focused.
6. Request and response pass-through eligibility remain independently representable.
7. No structural migration step intentionally changes protocol behavior.
8. Streaming consolidation happens only after buffered behavior is stable.
9. Shadow validation may compare pure planning decisions, but must never duplicate a live upstream request.
10. The cc-switch parity inventory must be COMPLETE before parity-sensitive behavior is moved.

## Consequences

### Positive

- The dispatcher consumes one conversion plan and one attempt outcome instead of knowing every strategy-specific condition.
- Strategy selection becomes directly testable and observable.
- Retry, health, usage, logging, and hook capability differences become explicit.
- Shared transport and lifecycle fixes apply consistently across strategies.
- Native IR remains extensible while Raw-Wire Compat retains its stronger wire contract.
- PassThrough becomes a first-class strategy instead of an emergent combination of Native mode and mutation flags.

### Negative

- The system intentionally retains more than one internal representation.
- The transitional period adds adapters before old branches can be deleted.
- Streaming requires owned Send + static state and cannot be unified cheaply.
- Hook mutation support differs by strategy and must be described as capabilities.
- Compatibility parity remains an additional maintenance authority alongside native codec tests.

## Rejected alternatives

### Replace Raw-Wire Compat with Native IR

Rejected because semantic equality does not guarantee ordered JSON, private carrier restoration, session behavior, header fidelity, SSE event order, retry timing, or client error-envelope parity.

### Replace Native IR with pair-specific compatibility transforms

Rejected because it returns the architecture to N×M conversion, weakens general protocol composability, and makes semantic hooks and future protocols much harder to support.

### Keep the current dispatcher branches indefinitely

Rejected because selection, lifecycle, retry, health, usage, and hook behavior continue to drift and accumulate in the dispatcher seam.

### Introduce a dynamic strategy plugin registry immediately

Rejected as premature. There are exactly three internal strategies with different concrete state. Closed enums give exhaustive matching and avoid async-trait, object-safety, associated-session, and downcast complexity.
