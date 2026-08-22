# Domain Context

## Provider Enablement

A persistent operator-controlled setting that determines whether a provider may participate in routing at all.

## Quota Scheduling Eligibility

A runtime decision derived from authoritative provider usage information that determines whether a provider may receive new requests. It does not change Provider Enablement or the provider's configured routing order.

## Quota Exhaustion

A condition in which at least one active provider quota window is fully consumed, or the provider explicitly reports that the account is unavailable. A quota-exhausted provider is temporarily ineligible for new requests.

## Confirmed Recovery

A successful provider usage observation showing that every active quota window is below its limit and that the account is not explicitly unavailable. Only Confirmed Recovery restores Quota Scheduling Eligibility after Quota Exhaustion.

## Protocol Negotiation

A deterministic decision that selects the upstream protocol endpoint a provider will receive. Protocol Negotiation does not decide how the request and response are converted.

## Protocol Conversion

The bidirectional capability that prepares a client request for an upstream protocol and converts the upstream response back to the client protocol while preserving the selected behavioral contract.

## Conversion Plan

A per-provider-attempt decision made after Protocol Negotiation and provider selection. It selects the Conversion Strategy using the effective provider, model, request, and mutation requirements, and is recomputed when a retry selects a different provider target.

## Conversion Strategy

One of the mutually explicit behavioral approaches used by a Conversion Plan: PassThrough, Native IR, or Raw-Wire Compat.

## PassThrough

A Conversion Strategy that skips cross-protocol semantic translation when the client and upstream protocols align. It may still apply explicitly allowed provider, model, authentication, and protocol-default adjustments; it does not promise byte-identical request forwarding.

## Native IR

A Conversion Strategy that translates through Nyro's normalized protocol meaning. It is the general-purpose strategy for composable cross-protocol conversion and semantic hooks.

## Raw-Wire Compat

A Conversion Strategy that performs pair-specific, stateful wire conversion for externally observable compatibility, including raw payload shape, event order, headers, session behavior, and client-specific errors.
