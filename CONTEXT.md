# Domain Context

## Language

**Provider**:
An operator-configured upstream service offered as one routing destination, with shared model and connection policies. A Provider is not the identity of an upstream subscriber.
_Avoid_: Account when referring to a routing destination

**Provider Account**:
An upstream subscriber identity attached to a Provider, with its own authorization and applicable usage entitlements. Logging in again changes its authorization, not necessarily its identity or remaining quota.
_Avoid_: Provider, API key

**Provider Account Pool**:
The Provider Accounts available within one Provider for serving requests under that Provider's shared policies. Pool membership does not create additional model-routing destinations.
_Avoid_: Provider group

**Account Enablement**:
An operator-controlled setting determining whether a Provider Account may participate in its pool. Temporary quota or authorization problems do not change this setting.
_Avoid_: Account health

**Homogeneous Plan Pool**:
A Provider Account Pool whose members share the same subscription tier and equal included entitlements for each applicable quota window. A Devin pool is designated Pro, with daily and weekly windows, or Max, with a weekly window only.
_Avoid_: Mixed-plan pool

**Account Quota Unit**:
One full included allowance for one account of a specified plan and quota window. Units add across distinct, independent accounts of that same plan and window, but daily units, weekly units and different plans are not interchangeable.
_Avoid_: Token allowance when no token-denominated capacity is known

**Pool Quota Utilization**:
The sum of consumed Account Quota Units divided by the total included capacity of the pool's accounting members for that window. This aggregate describes quota consumption, not the ability of any particular account to accept a request now.
_Avoid_: Scheduling eligibility

## Provider Enablement

A persistent operator-controlled setting that determines whether a provider may participate in routing at all.

## Quota Scheduling Eligibility

A runtime decision derived from authoritative upstream usage information that determines whether an account or a provider may receive new requests. It does not change operator-controlled enablement; one account losing eligibility does not by itself make its entire pool ineligible.

## Quota Exhaustion

A condition in which at least one applicable included quota window is fully consumed, or the upstream explicitly reports that the account is unavailable. Exhausted included quota prevents new requests unless eligible Reserve Coverage applies; explicit account unavailability cannot be overridden by a balance.

## Reserve Coverage

An upstream-supported paid balance that may carry requests after included quota is exhausted, when the operator's spending policy permits it. It is not restored included quota and is distinct from Confirmed Recovery.

## Confirmed Recovery

A successful authoritative usage observation showing that every applicable included quota window is below its limit and that the account is not explicitly unavailable. Elapsed reset time alone is not Confirmed Recovery.

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
