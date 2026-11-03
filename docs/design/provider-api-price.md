# Coding-plan API-equivalent prices

MiniMax, GLM, xAI/Grok and OpenAI Codex OAuth provider cards show three figures below their upstream quota windows:

- **Total API price:** the API-equivalent value of all retained upstream attempts for that provider ID.
- **Current-cycle API price:** the retained value within the selected upstream reset cycle.
- **Estimated cycle API capacity:** current-cycle value divided by that cycle's used fraction, projected to 100% quota use. This includes usage so far; it is **not the remaining balance**.

These are reference estimates using recorded tokens and the current automatic or manual model prices, not subscription payments or upstream invoices. Failed and retried attempts are included. Gross input already includes cache reads, which are not added a second time. Usage outside this gateway and unreported tokens cannot be recovered.

## Retention and pricing

Totals are scoped to the exact provider ID and upstream model, without requiring account-pool attribution. Clearing or deleting request logs reduces these figures. They intentionally follow account-pool retained-log statistics, not the permanent daily token-usage ledger.

USD, CNY and other native currency buckets remain separate. Missing reference prices display `—`, partial pricing displays `*` and explains the excluded attempts, and an unused cycle or explicitly priced zero displays zero. Small nonzero amounts retain the existing `<$0.01` / `<¥0.01` formatting. A runtime model catalog or a manual model price can supply prices absent from the embedded catalog; the feature never guesses a newer model's rate from an older version.

## Reset-cycle selection

The backend selects `weekly_limit` when reported, regardless of its position among the quota bars. If no weekly tier is reported, GLM, MiniMax and OpenAI Codex use an identified `five_hour` tier; Grok uses `monthly`. Feature/model-specific windows are not combined with the selected quota.

OpenAI Codex includes weekly-only Pro accounts. Window identity comes from the upstream duration, not the primary/secondary slot or plan name. Unknown-duration `primary_window` / `secondary_window` and feature-specific limits are not treated as a provider-wide cycle: retained total prices remain available, but current-cycle prices and capacity stay unknown. This applies to Codex OAuth subscriptions, not regular OpenAI API-key billing.

- Weekly: upstream reset minus seven days.
- Five-hour: upstream reset minus five hours.
- Monthly: upstream reset minus one calendar month in the reset timestamp's explicit offset, clamped at the previous month's last day. Date-only resets use UTC.

Grok date-only resets (`YYYY-MM-DD`) mean midnight UTC; RFC 3339 timestamps retain their instant. The interval is `[start, end)` and log reads are also capped at the captured upstream observation time. A present but duplicated, missing-reset, invalid or expired weekly tier remains unknown; it does not silently fall back to a short or calendar window. A zero used fraction can identify a cycle, but cannot produce a capacity estimate.

Only finite usage above 0% and at most 100%, with a priced sample from the same cycle, is projected. Invalid or overflowing estimates are withheld. The WebUI suppresses capacity estimates after a query error or when the quota observation is stale (five minutes), and invalidates current-cycle values at the reset boundary without rolling the cached reset forward. Tooltips show backend cycle bounds in the browser's local time and explain the projection's sensitivity to log coverage, external usage and model mix.

## Admin contract

`GET /api/v1/providers/:id/usage`, the bulk admin usage listing, and the existing desktop `get_provider_usage` command share the optional `api_price` field for `glm_coding_plan`, `minimax_coding_plan`, `grok_plan` and `openai_codex`:

| Field | Meaning |
| --- | --- |
| `all_time` | Retained totals: `total_tokens`, `request_count`, `equivalent_costs`, `unpriced_request_count`. |
| `period` | The same totals within a known live reset cycle; otherwise `null`. |
| `period_start`, `period_end` | Epoch-millisecond, half-open bounds; otherwise `null`. |
| `period_tier` | Selected canonical quota window; otherwise `null`. |
| `used_percent` | Finite observed usage in `[0, 100]` for that window, or `null`. |
| `estimated_capacity` | Projected native-currency cost buckets, or `null`; request counts describe the observed priced sample, not a forecast. |

A statistics/pricing failure omits the optional field without failing quota observation or changing routing eligibility. Other provider backends omit it. The quota monitor and proxy usage forwarding do not compute these prices; snapshots and forwarded responses exclude the admin-only field, including for privileged proxy keys. No new database schema or upstream credential mutation is involved.
