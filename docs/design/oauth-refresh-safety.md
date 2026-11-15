# OAuth refresh safety

[中文](<oauth-refresh-safety_CN.md>)

## Scope

Refreshes reuse the credential row and generation CAS in Memory, SQLite, PostgreSQL and MySQL. No schema migration or second token store is added. The refresh HTTP client disables redirects and transport retries. Authorization-code exchange is unchanged.

Token acquisition and publication run in a bounded detached task. Caller cancellation stops waiting, not rotation or publication. Before sending, the task verifies ownership of the exact refreshing generation.

## Failure classes

- **Pre-send:** local construction or connection establishment failed before sending the grant; retry is permitted.
- **Rejected:** an explicit standard OAuth refusal; the credential enters error and requires reauthorization.
- **Rate limited:** HTTP 429 persists a secret-free RFC3339 deadline under `nyro_refresh_retry_at`. Provider/account refresh CAS operations check it under their storage lock, including management and background entry points. Retry-After seconds/HTTP dates and Retry-After-Ms are supported, bounded to 1 second–1 hour, defaulting to 60 seconds.
- **Outcome unknown:** the grant may have rotated, but transport/read/decode/deadline or publication failed. Codex is quarantined rather than automatically replayed. Existing non-Codex transient error behavior is retained unless publication failed.

Stale Codex/OpenAI refreshing rows become errors during crash recovery: elapsed time does not prove the grant was never received. Other drivers retain their prior stale-recovery policy. Typed refresh errors retain no raw token responses, secrets, or request URLs.

## Publication and limits

Publication has a separate bounded budget. Retries write the same bundle with the same generation CAS, never issue another token request. An exact successor generation carrying the same token pair, identity metadata and unique publication ID confirms an acknowledgement-lost write. New authorizations clear that ID, even if their tokens happen to match. Disconnect/rebind/newer generations always fence stale work. Successful rotation and reauthorization clear the retry barrier; failed-bind compensation restores the previous barrier.

**This is not a global refresh journal.** If storage remains unavailable or the process exits before publication, no independent durable receipt retains the successful bundle. Codex fails closed and requires reauthorization. Identical grants imported into different providers or databases are not globally deduplicated. The protection is scoped to the existing account row/generation.

## Recovery

Wait for a 429 deadline; repeated refresh clicks cannot bypass it. For an unknown Codex outcome, sign in again and bind a fresh credential. Do not manually reset the row to connected or reuse the old refresh token. A valid Claude access token is not revoked merely because refreshing it was rate-limited; existing safe routing behavior is retained.

## Verification

New fixtures use synthetic credentials and loopback HTTP or fake drivers; no production token is required.

```sh
cargo test --offline -p nyro-core --lib refresh_safety_tests
cargo test --offline -p nyro-core --lib auth::refresh
cargo test --offline -p nyro-core --lib refresh_http
cargo test --offline -p nyro-core --lib storage::oauth_refresh_tests
cargo check --offline -p nyro-core
```

Coverage includes lost responses, malformed JSON, explicit refusals, Retry-After formats, cancellation, reauthorization racing publication, stale Codex leases, metadata-only CAS, and cross-account barrier isolation.
