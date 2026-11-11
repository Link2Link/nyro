# Claude Code generated wire profile

[中文](<claude-code-wire-profile_CN.md>)

## Scope

The Anthropic `claude-code` channel shapes non-native clients at `egress::prepare`, after the dispatcher has merged provider/client headers and built the final protocol body. This applies to Messages, streaming Messages, and the channel token-count path; ordinary Anthropic API-key providers do not enter this adapter. Admin model probes use the same adapter.

This is protocol compatibility, not a guarantee of account safety, subscription eligibility, or cache hits. Verification uses local fixtures; it does not authorize an otherwise unsupported upstream use.

## Coherent generated profile

The generated profile is one versioned unit: CLI **2.1.321**, Stainless **0.128.0**, runtime **v26.12.1**, with the matching generated billing version and request-derived beta rules. Defaults and capability ordering follow the CLIProxyAPI `b4e7dbd2` reference, rather than the earlier version-only UA bump.

Non-native callers cannot override individual generated identity headers through incoming or premerged headers: the adapter rebuilds them and validates the final tuple and billing version. There is no new per-field profile configuration or learning from caller headers. Confirmed native clients retain the existing, independent 2.1.x patch >= 280 compatibility range; the strict generated-profile validator is not applied to their software identity.

Betas are derived from the shaped request, model, credential, and request category. Quota probes differ from normal Messages; thread objects request the thread beta; one-hour Messages cache markers require the extended-cache beta. Token counting retains its own beta list and does not acquire the Messages cache policy. Unknown beta forwarding retains the existing gateway policy.

## Cache normalization

For non-native Messages in automatic mode:

1. Materialize a valid top-level `cache_control` on the last eligible content block, falling back to system or a non-deferred tool. Delete the top-level marker only after finding a target. An explicit target marker wins; a bare ephemeral target can inherit the more specific top-level marker.
2. Add default breakpoints without creating a duplicate rolling message marker after materialization.
3. Enforce four markers total, reserving one for a non-null `thread`. Remove markers, never content. Count only actual top-level, tools, system, and message-content positions, not fields inside tool schemas or tool-result data.
4. Upgrade missing TTLs on ordinary main requests to one hour. Subagents default to five minutes unless a real caller cache marker asks for one hour; probes/helpers keep their separate policy.
5. Traverse tools -> system -> messages -> any remaining top-level marker. After a five-minute/default marker, downgrade subsequent one-hour markers to the default TTL. Explicit five-minute markers are not upgraded merely to accommodate a later marker.

`prompt_cache_options.mode: "explicit"` opts out of automatic placement and TTL rewriting. It does not disable caching or the breakpoint budget. The internal option is removed before sending. Invalid explicit TTL/order combinations are rejected locally rather than silently rewritten. An explicit one-hour Messages probe/helper is also rejected if its request category suppresses the required extended-cache beta.

The internal `count_tokens` shaping branch does not run the full Messages cloak, top-level materialization, or one-hour upgrade. This change does not add a public `/v1/messages/count_tokens` route; that branch is covered by adapter-level tests. Existing cache markers participate in the shared budget and, outside explicit mode, TTL-order normalization. Confirmed native Messages likewise retain ownership of marker placement, with the existing budget/normalization behavior and the explicit-mode exception.

## Final validation and signing

At the end of egress preparation, non-native requests receive read-only validation of:

- generated identity headers and the billing CLI version (billing is not required on token counting);
- cache-control object/type/TTL at actual cache positions;
- breakpoint budget and mixed-TTL ordering;
- one-hour Messages markers paired with the extended-cache beta.

Errors identify the field without including request text or credentials. The dispatcher returns a local 400 before upstream inference and does not retry another account for this failure. Validation does not mutate the body. The existing transport hook subsequently signs the final serialized bytes and restores tool aliases in responses.

Nyro channel shaping is **after** generic payload/header preparation; this is not CLIProxyAPI's separate final user-payload-rule barrier. Adding later overrides must also move or repeat read-only validation at that boundary; do not silently run normalization again after an explicit user override.

## Verification

```bash
cargo test -p nyro-core --lib provider::anthropic::claude_code
cargo test -p nyro-core --test claude_code_oauth
cargo check -p nyro-core
```

Coverage includes generated-profile consistency, beta fixtures, native version preservation, top-level cache controls, tool-result continuation, streaming, explicit mode, probes/subagents, thread budgets, token counting, and local rejection without upstream replay. Neither fixture results nor the static profile claim equivalence to every behavior of the official CLI.
