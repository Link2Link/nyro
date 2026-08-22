use std::fmt;

use thiserror::Error;

use crate::protocol::ids::ProtocolId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversionKind {
    PassThrough,
    NativeIr,
    RawWireCompat,
}

impl ConversionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PassThrough => "pass_through",
            Self::NativeIr => "native_ir",
            Self::RawWireCompat => "raw_wire_compat",
        }
    }
}

impl fmt::Display for ConversionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestConversionMode {
    PassThroughJson,
    IrEncode,
    RawWire,
}

impl RequestConversionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PassThroughJson => "pass_through_json",
            Self::IrEncode => "ir_encode",
            Self::RawWire => "raw_wire",
        }
    }
}

impl fmt::Display for RequestConversionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResponseConversionMode {
    PassThroughBytes,
    IrDecodeEncode,
    RawWire,
}

impl ResponseConversionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PassThroughBytes => "pass_through_bytes",
            Self::IrDecodeEncode => "ir_decode_encode",
            Self::RawWire => "raw_wire",
        }
    }
}

impl fmt::Display for ResponseConversionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversionCapabilities {
    pub request_semantic_mutation: bool,
    pub request_wire_patch: bool,
    pub buffered_semantic_response_mutation: bool,
    pub stream_semantic_response_mutation: bool,
    pub opaque_wire_observation: bool,
}

impl ConversionCapabilities {
    pub const PASS_THROUGH: Self = Self {
        request_semantic_mutation: false,
        request_wire_patch: false,
        buffered_semantic_response_mutation: false,
        stream_semantic_response_mutation: false,
        opaque_wire_observation: true,
    };

    pub const NATIVE_IR: Self = Self {
        request_semantic_mutation: true,
        request_wire_patch: false,
        buffered_semantic_response_mutation: true,
        stream_semantic_response_mutation: true,
        opaque_wire_observation: false,
    };

    pub const RAW_WIRE_COMPAT: Self = Self {
        request_semantic_mutation: false,
        request_wire_patch: true,
        buffered_semantic_response_mutation: true,
        stream_semantic_response_mutation: true,
        opaque_wire_observation: true,
    };

    pub const fn for_kind(kind: ConversionKind) -> Self {
        match kind {
            ConversionKind::PassThrough => Self::PASS_THROUGH,
            ConversionKind::NativeIr => Self::NATIVE_IR,
            ConversionKind::RawWireCompat => Self::RAW_WIRE_COMPAT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassThroughPlan {
    ingress: ProtocolId,
    egress: ProtocolId,
    rule_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIrPlan {
    ingress: ProtocolId,
    egress: ProtocolId,
    request_mode: RequestConversionMode,
    response_mode: ResponseConversionMode,
    rule_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWireCompatPlan {
    ingress: ProtocolId,
    egress: ProtocolId,
    rule_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionPlan {
    PassThrough(PassThroughPlan),
    NativeIr(NativeIrPlan),
    RawWireCompat(RawWireCompatPlan),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConversionPlanError {
    #[error("PassThrough requires identical ingress and egress protocols ({ingress} != {egress})")]
    PassThroughProtocolMismatch {
        ingress: ProtocolId,
        egress: ProtocolId,
    },
    #[error("Native IR plan must contain at least one semantic conversion leg")]
    NativeIrWithoutSemanticLeg,
    #[error("Native IR plan cannot contain raw-wire conversion modes")]
    NativeIrWithRawWireLeg,
}

impl ConversionPlan {
    pub fn pass_through(
        ingress: ProtocolId,
        egress: ProtocolId,
        rule_id: impl Into<String>,
    ) -> Result<Self, ConversionPlanError> {
        if ingress != egress {
            return Err(ConversionPlanError::PassThroughProtocolMismatch { ingress, egress });
        }
        Ok(Self::PassThrough(PassThroughPlan {
            ingress,
            egress,
            rule_id: rule_id.into(),
        }))
    }

    pub fn native_ir(
        ingress: ProtocolId,
        egress: ProtocolId,
        request_mode: RequestConversionMode,
        response_mode: ResponseConversionMode,
        rule_id: impl Into<String>,
    ) -> Result<Self, ConversionPlanError> {
        if request_mode == RequestConversionMode::RawWire
            || response_mode == ResponseConversionMode::RawWire
        {
            return Err(ConversionPlanError::NativeIrWithRawWireLeg);
        }
        if request_mode == RequestConversionMode::PassThroughJson
            && response_mode == ResponseConversionMode::PassThroughBytes
        {
            return Err(ConversionPlanError::NativeIrWithoutSemanticLeg);
        }
        Ok(Self::NativeIr(NativeIrPlan {
            ingress,
            egress,
            request_mode,
            response_mode,
            rule_id: rule_id.into(),
        }))
    }

    pub fn raw_wire_compat(
        ingress: ProtocolId,
        egress: ProtocolId,
        rule_id: impl Into<String>,
    ) -> Self {
        Self::RawWireCompat(RawWireCompatPlan {
            ingress,
            egress,
            rule_id: rule_id.into(),
        })
    }

    pub const fn kind(&self) -> ConversionKind {
        match self {
            Self::PassThrough(_) => ConversionKind::PassThrough,
            Self::NativeIr(_) => ConversionKind::NativeIr,
            Self::RawWireCompat(_) => ConversionKind::RawWireCompat,
        }
    }

    pub const fn ingress(&self) -> ProtocolId {
        match self {
            Self::PassThrough(plan) => plan.ingress,
            Self::NativeIr(plan) => plan.ingress,
            Self::RawWireCompat(plan) => plan.ingress,
        }
    }

    pub const fn egress(&self) -> ProtocolId {
        match self {
            Self::PassThrough(plan) => plan.egress,
            Self::NativeIr(plan) => plan.egress,
            Self::RawWireCompat(plan) => plan.egress,
        }
    }

    pub fn rule_id(&self) -> &str {
        match self {
            Self::PassThrough(plan) => &plan.rule_id,
            Self::NativeIr(plan) => &plan.rule_id,
            Self::RawWireCompat(plan) => &plan.rule_id,
        }
    }

    pub const fn request_mode(&self) -> RequestConversionMode {
        match self {
            Self::PassThrough(_) => RequestConversionMode::PassThroughJson,
            Self::NativeIr(plan) => plan.request_mode,
            Self::RawWireCompat(_) => RequestConversionMode::RawWire,
        }
    }

    pub const fn response_mode(&self) -> ResponseConversionMode {
        match self {
            Self::PassThrough(_) => ResponseConversionMode::PassThroughBytes,
            Self::NativeIr(plan) => plan.response_mode,
            Self::RawWireCompat(_) => ResponseConversionMode::RawWire,
        }
    }

    pub const fn capabilities(&self) -> ConversionCapabilities {
        ConversionCapabilities::for_kind(self.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, OPENAI_RESPONSES_V1,
    };

    #[test]
    fn pass_through_requires_matching_protocols() {
        let error = ConversionPlan::pass_through(
            OPENAI_RESPONSES_V1,
            ANTHROPIC_MESSAGES_2023_06_01,
            "invalid",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConversionPlanError::PassThroughProtocolMismatch { .. }
        ));
    }

    #[test]
    fn pass_through_exposes_stable_modes_and_capabilities() {
        let plan = ConversionPlan::pass_through(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            "native-no-mutations",
        )
        .unwrap();
        assert_eq!(plan.kind(), ConversionKind::PassThrough);
        assert_eq!(plan.request_mode(), RequestConversionMode::PassThroughJson);
        assert_eq!(
            plan.response_mode(),
            ResponseConversionMode::PassThroughBytes
        );
        assert!(plan.capabilities().opaque_wire_observation);
        assert!(!plan.capabilities().request_semantic_mutation);
    }

    #[test]
    fn native_ir_requires_at_least_one_semantic_leg() {
        let error = ConversionPlan::native_ir(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            RequestConversionMode::PassThroughJson,
            ResponseConversionMode::PassThroughBytes,
            "invalid",
        )
        .unwrap_err();
        assert_eq!(error, ConversionPlanError::NativeIrWithoutSemanticLeg);
    }

    #[test]
    fn native_ir_supports_mixed_request_and_response_legs() {
        let plan = ConversionPlan::native_ir(
            OPENAI_RESPONSES_V1,
            OPENAI_RESPONSES_V1,
            RequestConversionMode::IrEncode,
            ResponseConversionMode::PassThroughBytes,
            "native-with-request-mutation",
        )
        .unwrap();
        assert_eq!(plan.kind(), ConversionKind::NativeIr);
        assert_eq!(plan.request_mode(), RequestConversionMode::IrEncode);
        assert_eq!(
            plan.response_mode(),
            ResponseConversionMode::PassThroughBytes
        );
        assert!(plan.capabilities().request_semantic_mutation);
        assert!(plan.capabilities().stream_semantic_response_mutation);
    }

    #[test]
    fn raw_wire_compat_is_owned_and_patch_capable() {
        let plan = ConversionPlan::raw_wire_compat(
            ANTHROPIC_MESSAGES_2023_06_01,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            "anthropic-to-chat",
        );
        let cloned = plan.clone();
        assert_eq!(cloned.kind(), ConversionKind::RawWireCompat);
        assert_eq!(cloned.request_mode(), RequestConversionMode::RawWire);
        assert_eq!(cloned.response_mode(), ResponseConversionMode::RawWire);
        assert!(cloned.capabilities().request_wire_patch);
        assert_eq!(cloned.rule_id(), "anthropic-to-chat");
    }

    #[test]
    fn strategy_and_leg_names_are_stable_for_diagnostics() {
        assert_eq!(ConversionKind::PassThrough.as_str(), "pass_through");
        assert_eq!(ConversionKind::NativeIr.as_str(), "native_ir");
        assert_eq!(ConversionKind::RawWireCompat.as_str(), "raw_wire_compat");
        assert_eq!(
            RequestConversionMode::PassThroughJson.as_str(),
            "pass_through_json"
        );
        assert_eq!(
            ResponseConversionMode::IrDecodeEncode.as_str(),
            "ir_decode_encode"
        );
    }
}
