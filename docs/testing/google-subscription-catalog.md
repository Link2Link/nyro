# Google subscription catalog policy

This covers catalog discovery for both Google subscription channels and the Antigravity login, session, and throttling behavior tested alongside it.

## Endpoint and identity

- Antigravity defaults inference and `fetchAvailableModels` to `https://daily-cloudcode-pa.googleapis.com`.
- Gemini CLI defaults to `https://cloudcode-pa.googleapis.com`.
- A provider base URL other than the historical prod preset is an explicit override used by the Google auth runtime binding. An existing Antigravity prod preset remains a legacy default, not evidence of an intentional prod override.
- Admin discovery, probes, and usage honor the effective runtime base URL and User-Agent. They never silently retry another host. OAuth refresh receives the effective endpoint through its context; initial OAuth exchange uses the channel default.
- Internal credential metadata may retain an explicit `subscription_endpoint`. This is an integration detail, not a new UI setting.

Project discovery (`loadCodeAssist`) and first-time onboarding (`onboardUser`) both use the channel's trusted control plane: daily for Antigravity, prod for Gemini CLI. A custom inference/catalog URL does not redirect these calls. HTTP 200 with a completed operation carrying an error is a failed login, not a missing project. Diagnostics report recognized reasons such as `VALIDATION_REQUIRED` without exposing upstream messages or signed links.

## Snapshot lifecycle

Refresh-only budgets protect successful token rotation: token preparation (including any Hub manifest work) has a 20-second limit, then optional project recovery and catalog discovery share a 2-second best-effort limit. An enrichment error or timeout preserves the successful token bundle; a still-missing project remains an explicit request-time diagnostic. These budgets do not apply to initial login.

Credential metadata keeps `subscription_models` beside `subscription_models_source`. Source format version 1 binds IDs to endpoint, project ID, full User-Agent (including version), and a SHA-256 credential-token fingerprint. The fingerprint is not the token and is not included in probe diagnostics. Old source-less or mismatched snapshots are unknown: changing project, endpoint, client identity, or token cannot reuse their dynamic IDs. Curated aliases and manual model IDs remain usable.

Auth and admin write the same snapshot format. Admin captures the connected row and generation **before** the asynchronous fetch, checks it matches the runtime, and writes metadata using `update_meta_if_version`. Refresh, logout/rebind, or concurrent catalog writes defeat stale writes. CAS never replaces tokens, resets account state, or changes refresh timestamps. Unchanged metadata is not rewritten.

| Fetch result | Dynamic snapshot | Caller semantics |
| --- | --- | --- |
| Valid nonempty models map | Replace IDs and source | Successful discovery |
| Valid empty models map | Replace IDs with empty array and source | Successful empty discovery, not outage |
| HTTP error, malformed JSON/map/entry, timeout | Do not change snapshot | Best effort: unknown/fallback; strict catalog caller: error |

Fresh successful catalogs replace old dynamic probe evidence even when empty. Failed discovery may reuse a matching last-good snapshot. Neither an empty directory nor an absent model proves lack of entitlement; curated discovery additions remain, and manual selections are still probed as requested.

## Live observation: Gemini 4 Hydrogen / Argon (2026-11-15)

Probed from the US VPS egress with Hub UA `antigravity/hub/2.32.1` on two Google AI Pro accounts:

- Entitled account, daily host: the catalog lists `gemini-4-hydrogen-{low,medium,high,xhigh}` and `gemini-4-argon-{low,medium,high}` (plus a `gemini-4-carbon-disabled` Ultra placeholder). Every tier answered 200 on `streamGenerateContent`; the bare `gemini-4-hydrogen` / `gemini-4-argon` ids answered 404. Bare names therefore stay virtual aliases resolved by effort (`xhigh`/`max` → `-xhigh` on Hydrogen only; no directive → `-medium`).
- Same account, prod host: the catalog omits Hydrogen and Hydrogen inference answers 429. This is why catalog discovery must follow the daily inference host.
- Quota: Hydrogen calls drew down `gemini-5h` / `gemini-weekly` ("Gemini Models", shared with 3.8 Flash); Argon calls drew down only `gemini-argon-wtu-weekly`.
- Account outside the plan: only `gemini-4-{argon,hydrogen,carbon}-disabled` placeholders; tier ids answer 404.
- System-prompt screening: a third-party agent identity in `systemInstruction` ("You are an AI agent powered by DeepSeek Harness.", the Claude Code identity) is answered with a detail-free 429 `RESOURCE_EXHAUSTED` on every model, while the same text as a leading user turn is served. Nyro therefore always relocates `systemInstruction` on this channel, and detail-free 429s no longer cool the model down. Not a guarantee: a Codex CLI identity is refused in either position.

## Neon catalog and quota display

Antigravity discovery also reports `gemini-4-neon-{low,medium,high,xhigh}`. Nyro presents these as `gemini-4-neon` and maps effort to the tier; no directive selects `-medium`. The three-pool usage layout places Neon alongside Hydrogen in **Gemini Models**; see the [quota contract](../design/quota-buckets.md). The Neon mapping is covered by offline tests, not by the November 15 live probes above.

## Probe diagnostics

Existing result errors include public model ID, effective wire model ID, and endpoint-source label; existing run metadata contains the actual base URL. Debug logs carry model mapping and endpoint source without the credential fingerprint or project ID. A 404 hint lists possible model, endpoint/project, client-version, and eligibility causes rather than declaring a subscription plan unsupported. Quota readouts likewise do not prove a 429 cause.

## Offline regressions

- Antigravity preserves an explicit native Gemini `sessionId`. Otherwise it derives the upstream ID from a client conversation identifier or the first nonblank user text, scoped to provider, account, and project. Appending a turn or renewing the access token does not change the ID. Without either input, it generates a fresh ID.
- For HTTP 429, a structured body delay takes priority over `Retry-After` (integer seconds or HTTP date). Explicit zero adds no model wait; a bare `RESOURCE_EXHAUSTED` without quota evidence is not grounds for a model cooldown. Explicit waits apply only to the effective upstream model and current owner generation, with a seven-day safety bound.
- A Gemini error frame inside an HTTP 200 stream is terminal. Before delivery or during aggregation, return an error response. After delivery starts, emit a client-protocol error and never append a success marker or replay the request.

Run serially from the workspace root. New HTTP fixtures use loopback and synthetic credentials, never real Google endpoints/accounts.

```sh
cargo test -p nyro-core google_catalog
cargo test -p nyro-core provider::google::antigravity::tests::catalog_
cargo test -p nyro-core google_subscription_
cargo test -p nyro-core --lib auth::drivers::google::tests
cargo test -p nyro-core --lib antigravity_session_is_stable
cargo test -p nyro-core --test google_subscription_non_stream
cargo test -p nyro-core subscription_model_catalog_keeps_wire_ids_and_channel_specific_fallbacks
cargo check -p nyro-core
```

Coverage: daily/prod defaults, historical prod compatibility, explicit provider/runtime bindings, request path/project/UA, malformed-versus-empty responses, endpoint/project/UA/token invalidation, auth/admin source parity, failure retention, fresh empty probe evidence, and asynchronous fetch-versus-rebind races. Storage CAS tests separately cover stale/refreshing generations and account-state preservation. These tests do not certify live model availability or plan eligibility.
