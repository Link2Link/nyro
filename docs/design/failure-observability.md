# Failure Observability and Attempt Outcomes

[简体中文](failure-observability_CN.md) · [Database schema](../database/schema.md)

This document describes the request-log diagnostic contract. It is separate from
the proposed OpenTelemetry framework in [observability.md](observability.md).
The implementation lives in `nyro-core`: `logging/diagnostics.rs`,
`logging/payload.rs`, the proxy/response lifecycle observers, storage, and the admin
service. HTTP and desktop administration consume the same core semantics.

## Scope and invariants

- Preserve evidence for confirmed failures, timeouts, cancellations, and output
  limits even when ordinary payload recording is disabled.
- Distinguish a failed attempt from the eventual result of a client request that
  may have retried. Do not add final-result rows to attempt statistics.
- Treat missing or unsupported evidence as **unknown**, not success or failure.
- Keep existing TPS calculations, token/quota accounting, routing, retry policy,
  and the bytes sent on the wire unchanged. Diagnostic capture limits are not
  request/response size limits.
- Logging remains bounded, asynchronous, and best-effort. This feature does not
  promise lossless persistence or recovery of historical bodies/outcomes.

## Authoritative outcomes

`outcome_version = 1` is the only currently recognized authority for
`attempt_outcome`. It is independent of `performance_metadata_version`,
`request_completion`, token usage, and other older performance evidence. The
version alone is not proof of success: the outcome must also be recognized.

The effective error predicate, shared by display, error counts, and **Clear error
logs**, is:

```text
is_error = (400 <= client_status_code <= 599)
        OR (400 <= upstream_status_code <= 599)
        OR (outcome_version == 1 AND attempt_outcome IN {failed, timed_out})
```

A NULL status, 600, or a missing HTTP response does not satisfy the HTTP clause.
An upstream 429/500 is an error even if the client received HTTP 200. Genuine HTTP
errors continue to count for legacy rows and for unrecognized outcome versions.
An old `request_completion = failed`, by itself, does not establish an error.

Classification is exclusive and ordered:

| Effective outcome | Condition |
|---|---|
| `error` | The error predicate above is true; it takes priority over every other outcome |
| `completed` | Not an error, version 1, and `attempt_outcome = completed` |
| `cancelled` | Not an error, version 1, and `attempt_outcome = cancelled` |
| `output_limited` | Not an error, version 1, and `attempt_outcome = output_limited` |
| `unknown` | Everything else, including absent authority, legacy-only markers, and unsupported versions/values |

Confirmed completion requires appropriate upstream protocol terminal evidence
and gateway-confirmed full delivery: either a cleanly observed response Body
end-of-stream, or proof that the HTTP layer consumed **every client frame the
relay produced** (frame-count reconciliation). The latter is a required
supplement: SSE clients (codex CLI and similar) may close the connection the
moment they read the protocol terminal event — the response-body terminal EOS
poll and the upstream EOF poll then both lose the race against that close.
When the upstream terminal was parsed unambiguously and every frame was
delivered, those drop artifacts reconcile to confirmed completion instead of
cancellation. HTTP 200, nonzero tokens, or a synthetic converted completion
marker is insufficient; Body EOS is not an ACK from the client application.
A confirmed error or timeout is never demoted by delivery reconciliation or
overwritten by a later cancellation, output limit, or unknown observation. A
pure cancellation or output limit is neither a confirmed success nor an error.

A supported protocol's explicit error, transport/read/decompression failure,
conversion failure, timeout, or missing required terminal can establish failure
when the lifecycle observer has that evidence. A diagnostic observer overflowing
its bounded parse window or encountering an unsupported dialect cannot establish
a protocol error: its result stays unknown. Capture truncation is also not itself
a transport or protocol failure.

### Counts and rates

Provider, API-key, and model usage details expose `outcome_stats_version = 1` and
five exclusive counters:

```text
request_count = success_count + error_count + cancelled_count
              + output_limited_count + unknown_count

confirmed success rate = success_count / request_count
unknown rate           = unknown_count / request_count
```

The denominator includes **all retained attempts in the selected window**, not
just known outcomes and not distinct client requests. Display no-data handling
for zero attempts instead of dividing by zero. `success_count` means confirmed
`completed`, not HTTP 2xx or `request_count - error_count`. The UI exposes unknown
counts/rates so that a low confirmed success rate is not mistaken for a high
failure rate. Existing grouped/time-bucket `error_count` uses the same predicate;
latency, TPS, and token formulas are unchanged.

Legacy fixtures and migrated rows keep their absence of authority. For example,
HTTP 200, 302, 404, and 500 with version 0 produce total 4, success 0, error 2,
unknown 2. Do not backfill version 1 merely to preserve an older success count.

## Identity, correlation, and request results

Each log has a stable `id` allocated before persistence and preserved across
builder/fallback copies. `client_request_id` correlates the client request;
`attempt_index` distinguishes its upstream attempts. Legacy rows may lack both.
Attempt failure remains visible even if a later attempt succeeds.

The separate `request_results` table holds one final result per
`client_request_id`: `final_outcome`, `final_attempt_id`, `attempt_count`, and
`finished_at`. It is joined into an admin log detail as optional `request_result`;
this is not another request-log row and contributes **no** request, success,
error, TPS, token, or quota count. Final correlation can be absent when no final
record was persisted; absence is not success. There is deliberately no foreign
key from `final_attempt_id` to a log: deleting that attempt need not discard the
final result while sibling attempts remain.

Public `RequestLog` contains derived `is_error` and `effective_outcome`, plus
`failure_kind`, `failure_stage`, a bounded sanitized `error_message`, and
`error_causes`. `error_causes` and `payload_metadata` are nullable **JSON-encoded
strings** in the admin wire object, not embedded arrays/objects; clients parse
once. Their physical columns are `error_causes_json` and
`payload_metadata_json`. Summary/list queries omit raw payloads; authorized admin
detail views can inspect retained content and correlation.

## Bounded payload evidence

The four body directions are independent: client request, upstream request,
upstream response, and client response. For each direction:

- Retain at most **1 MiB of raw bytes**: the first **512 KiB** and last
  **512 KiB**. Smaller bodies retain all observed bytes.
- Do not insert a synthetic separator into stored content. A truncated body is a
  head/tail capture, not a replayable original body; metadata describes the split.
- Preserve retained bytes exactly. Store valid UTF-8 as text; otherwise use
  Base64, including when a head/tail cut splits a UTF-8 character. The Base64
  string may occupy up to 1,398,104 bytes while the raw-byte cap remains 1 MiB.
- A disconnect/read failure can leave only partial bytes. Unobserved directions
  are absent, not fabricated empty bodies. An explicitly observed empty body is
  distinguishable from absence.

Each of the four header blocks has a separate **64 KiB serialized JSON** ceiling;
the shared implementation retains at most **65,535 bytes** to fit MySQL `TEXT`
without backend-specific loss at the boundary. Redaction happens before retention. Whole fields that do not fit are omitted so
the stored JSON stays valid; protocol, request-ID, and rate-limit headers are
prioritized. Header metadata reports omitted/redacted fields rather than
pretending the block is complete.

The final retention gate is:

```text
retain = is_error
      OR (outcome_version == 1 AND attempt_outcome IN {cancelled, output_limited})
      OR (global_enable_payload AND model_enable_payload.unwrap_or(true))
```

Thus confirmed abnormal outcomes override both ordinary switches. Ordinary
completed/unknown attempts obey the global/model gate and use the **same caps**
when retained. A model override does not bypass a disabled global switch for
ordinary traffic. Forced retention means retaining the bounded evidence that was
actually observed, not guaranteeing four complete bodies or durable persistence.

### Metadata and UI interpretation

`payload_metadata` is keyed by the eight payload field names (for example
`client_request_body` or `upstream_response_headers`). Entries describe:

- `capture_state`: `captured`, `empty`, `absent`, or `not_retained`;
- `complete`: whether the observed direction completed; independent of truncation;
- `total_observed_bytes`, `retained_bytes`, and `truncated`;
- `encoding`: `utf8`, `base64`, or `none` as appropriate;
- body `head_bytes`/`tail_bytes`, or header `total_headers`, `retained_headers`,
  `omitted_headers`, and `redacted_headers`.

The UI distinguishes full, partial, absent, truncated, Base64, and administratively
cleared data rather than treating every non-null string as a complete body.
Observed-byte counts do not predict bytes never received. Header observed bytes
count original names/values, whereas retained bytes count redacted JSON; these
are not directly comparable sizes. Metadata does not contain another raw body.

## Privacy and destructive operations

Authentication/cookie/custom credential headers and credential-bearing URLs are
redacted for logs. Error chains are bounded and sanitized; they must not become a
second path for dumping raw bodies or credentials. **Bodies are intentionally raw
within the cap**, not recursively scrubbed: prompts, responses, personal data, or
credentials contained inside a body may remain. Inspection belongs only on the
authorized admin surface. Use appropriate admin access controls and retention.

**Clear payloads** explicitly clears all eight payload columns from every
payload-bearing log, **including error logs**. It preserves the row, stable ID,
classification, status, correlation, failure/capture metadata, usage, timing, and
routing metadata, and sets `payload_cleared_at` (Unix milliseconds). Capture
metadata then describes the historical capture, not content still available;
`payload_cleared_at` takes precedence in the UI. Repeating the clear with no
remaining payloads changes no rows.

**Clear error logs** deletes rows matching the shared effective error predicate.
It does not delete pure cancelled, output-limited, or unknown attempts. A legacy
HTTP 500 remains eligible, and an upstream HTTP 4xx is eligible even with client
HTTP 200. Single-log deletion and full-log clearing remain explicit operations.
No clearing operation can reconstruct already discarded evidence.

## Persistence failures and limits

The bounded log queue uses nonblocking enqueue. Queue-full and closed-channel
failures emit an explicit loss event with stable log/request identifiers when
available. Database batch-write failures emit an explicit loss event rather than
retrying invisibly. Process-local `logging_status()` reports
`queue_full_dropped`, `channel_closed_dropped`, `database_write_dropped`, and
`counts_reset_on_restart = true`.

There is **no retry queue, durable spool, or historical body/outcome recovery** in
this feature. Database-write loss accounting covers entries in the failed batch;
it is not a durable audit ledger or proof of exactly which rows a failing driver
committed. A crash can also lose in-flight evidence and resets the counters.
Operators must interpret incomplete logs together with those operational limits.

## Validation and schema artifacts

Existing logging/admin/usage/time-series tests cover conservative legacy counts,
all-payload clearing, and the HTTP error boundaries. `log_outcomes_storage` tests
exercise versioned outcomes and storage conformance with separately opted-in
empty disposable databases. The PostgreSQL/MySQL reference SQL is generated by
`nyro-tools dump-schema` only after final migrations are ready; see the
[safety and generation runbook](../database/schema.md#regenerating-reference-sql).
Never hand-edit generated SQL or point schema/test tooling at an application DB.
