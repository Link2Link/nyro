# Domain Context

## Provider Enablement

A persistent operator-controlled setting that determines whether a provider may participate in routing at all.

## Quota Scheduling Eligibility

A runtime decision derived from authoritative provider usage information that determines whether a provider may receive new requests. It does not change Provider Enablement or the provider's configured routing order.

## Quota Exhaustion

A condition in which at least one active provider quota window is fully consumed, or the provider explicitly reports that the account is unavailable. A quota-exhausted provider is temporarily ineligible for new requests.

## Confirmed Recovery

A successful provider usage observation showing that every active quota window is below its limit and that the account is not explicitly unavailable. Only Confirmed Recovery restores Quota Scheduling Eligibility after Quota Exhaustion.
