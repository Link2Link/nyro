//! Minimal, typed unary discovery messages, deliberately separate from Run wire types.
//!
//! Field numbers/types transcribed from the MIT-licensed can1357/oh-my-pi
//! revision 5d0aaffaf1f987503ce129a642fb6e30339615af:
//! packages/catalog/src/discovery/cursor-models.proto and cursor-proto.ts.
//! See PROTOCOL_SOURCES.md for the pinned sources and third-party MIT notice.
//! Unknown fields (including prices, aliases and migration suggestions) are ignored.
use super::wire;
use prost::Message;

pub const AVAILABLE_MODELS_PATH: &str = "/aiserver.v1.AiService/AvailableModels";
pub const GET_DEFAULT_MODEL_PATH: &str = "/agent.v1.AgentService/GetDefaultModelForCli";

#[derive(Clone, PartialEq, Message)]
pub struct AvailableModelsRequest {
    #[prost(bool, tag = "2")]
    pub include_long_context_models: bool,
    #[prost(bool, optional, tag = "5")]
    pub use_model_parameters: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    pub do_not_use_markdown: Option<bool>,
    #[prost(bool, optional, tag = "12")]
    pub use_cloud_agent_effort_modes: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AvailableModelsResponse {
    #[prost(message, repeated, tag = "2")]
    pub models: Vec<ModelDetails>,
    #[prost(bool, tag = "11")]
    pub use_model_parameters: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelDetails {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bool, optional, tag = "5")]
    pub supports_agent: Option<bool>,
    #[prost(bool, optional, tag = "9")]
    pub supports_thinking: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    pub supports_images: Option<bool>,
    #[prost(uint32, optional, tag = "15")]
    pub context_token_limit: Option<u32>,
    #[prost(uint32, optional, tag = "16")]
    pub context_token_limit_for_max_mode: Option<u32>,
    #[prost(string, optional, tag = "17")]
    pub client_display_name: Option<String>,
    #[prost(message, repeated, tag = "29")]
    pub parameter_definitions: Vec<ModelParameterDefinition>,
    #[prost(message, repeated, tag = "30")]
    pub variants: Vec<ModelVariantConfig>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelVariantConfig {
    #[prost(message, repeated, tag = "1")]
    pub parameter_values: Vec<wire::ModelParameter>,
    #[prost(string, optional, tag = "2")]
    pub display_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub is_max_mode: Option<bool>,
    #[prost(string, optional, tag = "11")]
    pub legacy_slug: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelParameterDefinition {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(message, optional, tag = "4")]
    pub parameter_type: Option<ModelParameterType>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelParameterType {
    #[prost(message, optional, tag = "1")]
    pub boolean_parameter: Option<BooleanParameterDefinition>,
    #[prost(message, optional, tag = "2")]
    pub enum_parameter: Option<EnumParameterDefinition>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BooleanParameterDefinition {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<BooleanParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BooleanParameterValue {
    #[prost(string, tag = "1")]
    pub value: String,
    #[prost(bool, optional, tag = "3")]
    pub increases_model_cost: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    pub blocked_by_admin_allowlist: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EnumParameterDefinition {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<EnumParameterValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EnumParameterValue {
    #[prost(string, tag = "1")]
    pub value: String,
    #[prost(bool, optional, tag = "3")]
    pub increases_model_cost: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub blocked_by_admin_allowlist: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetDefaultModelForCliRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct GetDefaultModelForCliResponse {
    #[prost(message, optional, tag = "1")]
    pub model: Option<wire::ModelDetails>,
}
