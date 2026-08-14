//! Raw-wire protocol compatibility with cc-switch.
//!
//! The mechanically ported conversion core is derived from cc-switch at
//! commit `eb69e4922ee187a261fd29c216a738e838f85bc4` under the MIT License.
//! See `THIRD_PARTY_LICENSES/cc-switch-MIT.txt`.
//!
//! This crate deliberately exposes bytes and small context/profile types rather
//! than `serde_json::Value`, keeping its ordered JSON implementation private at
//! the `nyro-core` boundary.

extern crate serde_json_ordered as serde_json;

pub mod engine;
pub mod profile;
pub mod session;
pub mod state;
pub mod transport;

pub(crate) mod ported;
pub(crate) mod provider;

pub use engine::{
    CompatEngine, CompatError, CompatStream, ConvertedResponse, PreparedRequest, ResponseBody,
    StreamStartDecision, codex_client_error_json, detect_semantic_failure,
};
pub use ported::content_encoding::{
    DecompressError, decompress_body, decompress_body_with_limit, get_content_encoding,
    is_supported_content_encoding,
};
pub use ported::providers::claude_compat::anthropic_normalization_needed;
pub use profile::{
    ClientSemantics, ConversionProfile, Direction, UpstreamFlavor, WireProtocol,
    resolve_chat_reasoning_config,
};
pub use provider::CodexChatReasoningConfig;
pub use session::{
    ConversionSession, SessionClient, SessionError, SessionIdentity, SessionSource,
    extract_session_identity,
};
pub use state::CompatState;
pub use transport::{
    BodyKind, Header, ResponseMetadata, body_diagnostics_suffix, body_looks_like_sse,
    classify_body, classify_body_for_diagnostics, should_force_identity_encoding,
    strip_hop_by_hop_headers,
};
