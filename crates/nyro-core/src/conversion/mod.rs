//! Unified protocol-conversion capability.
//!
//! Protocol negotiation remains in `proxy::planner`; this module owns the
//! second-stage, per-provider-attempt decision describing how request and
//! response wire shapes are handled. Execution adapters land incrementally.

mod outcome;
mod plan;
mod prepared;
mod resolver;
mod wire_patch;

pub(crate) use outcome::{ConversionAttempt, HealthDisposition, RetryDisposition};
pub use plan::{
    ConversionCapabilities, ConversionKind, ConversionPlan, ConversionPlanError, NativeIrPlan,
    PassThroughPlan, RawWireCompatPlan, RequestConversionMode, ResponseConversionMode,
};

pub(crate) use prepared::{
    PrepareConversionError, PrepareConversionInput, PreparedBody, PreparedSession,
    prepare_conversion, rebuild_raw_wire_request,
};
pub(crate) use resolver::{
    RawWireCompatSelection, ResolveConversionInput, ResolveRawWireCompatInput, resolve_conversion,
    resolve_raw_wire_compat, supports_raw_wire_compat,
};
#[cfg(test)]
pub(crate) use resolver::{chat_prompt_cache_key_supported, strip_one_m_suffix};
#[cfg(test)]
pub(crate) use wire_patch::request_patch;
