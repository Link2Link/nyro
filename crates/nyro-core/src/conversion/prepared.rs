use bytes::Bytes;
use nyro_ccswitch_compat::{CompatEngine, CompatError, ConversionSession, PreparedRequest};
use serde_json::Value;
use thiserror::Error;

use crate::protocol::ids::ProtocolId;

use super::resolver::{RawWireCompatSelection, ResolvedConversion};
use super::wire_patch::value_patch;
use super::{ConversionKind, ConversionPlan, ResponseConversionMode};

pub(crate) struct PrepareConversionInput<'a> {
    pub(crate) resolved: ResolvedConversion,
    pub(crate) engine: &'a CompatEngine,
    pub(crate) native_body: Value,
    pub(crate) raw_body: Option<Bytes>,
    pub(crate) vendor_wire_before: Option<&'a Value>,
}

#[derive(Debug, Error)]
pub(crate) enum PrepareConversionError {
    #[error("Raw-Wire Compat requires the original request body bytes")]
    MissingRawBody,
    #[error("Raw-Wire Compat requires the pre-vendor encoded request body")]
    MissingVendorWireBefore,
    #[error("resolved conversion strategy state is inconsistent with its plan")]
    StrategyStateMismatch,
    #[error(transparent)]
    RawWire(#[from] CompatError),
}

#[derive(Debug)]
pub(crate) enum PreparedBody {
    Json(Value),
    Raw(Bytes),
}

#[derive(Debug)]
pub(crate) enum PreparedSession {
    PassThrough,
    NativeIr {
        ingress: ProtocolId,
        egress: ProtocolId,
    },
    RawWireCompat(Box<ConversionSession>),
}

#[derive(Debug)]
pub(crate) struct PreparedConversion {
    plan: ConversionPlan,
    body: PreparedBody,
    force_upstream_stream: bool,
    session: PreparedSession,
}

impl PreparedConversion {
    pub(crate) fn plan(&self) -> &ConversionPlan {
        &self.plan
    }

    pub(crate) fn is_raw_wire(&self) -> bool {
        matches!(self.session, PreparedSession::RawWireCompat(_))
    }

    pub(crate) fn response_mode(&self) -> ResponseConversionMode {
        self.plan.response_mode()
    }

    pub(crate) fn into_parts(self) -> (ConversionPlan, PreparedBody, bool, PreparedSession) {
        (
            self.plan,
            self.body,
            self.force_upstream_stream,
            self.session,
        )
    }
}

pub(crate) async fn prepare_conversion(
    input: PrepareConversionInput<'_>,
) -> Result<PreparedConversion, PrepareConversionError> {
    let PrepareConversionInput {
        resolved,
        engine,
        native_body,
        raw_body,
        vendor_wire_before,
    } = input;
    let (plan, raw_wire) = resolved.into_parts();

    match (plan.kind(), raw_wire) {
        (ConversionKind::RawWireCompat, Some(selection)) => {
            prepare_raw_wire(
                plan,
                selection,
                engine,
                native_body,
                raw_body,
                vendor_wire_before,
            )
            .await
        }
        (ConversionKind::RawWireCompat, None) | (_, Some(_)) => {
            Err(PrepareConversionError::StrategyStateMismatch)
        }
        (ConversionKind::PassThrough, None) => Ok(PreparedConversion {
            plan,
            body: PreparedBody::Json(native_body),
            force_upstream_stream: false,
            session: PreparedSession::PassThrough,
        }),
        (ConversionKind::NativeIr, None) => {
            let ingress = plan.ingress();
            let egress = plan.egress();
            Ok(PreparedConversion {
                plan,
                body: PreparedBody::Json(native_body),
                force_upstream_stream: false,
                session: PreparedSession::NativeIr { ingress, egress },
            })
        }
    }
}

async fn prepare_raw_wire(
    plan: ConversionPlan,
    selection: RawWireCompatSelection,
    engine: &CompatEngine,
    native_body: Value,
    raw_body: Option<Bytes>,
    vendor_wire_before: Option<&Value>,
) -> Result<PreparedConversion, PrepareConversionError> {
    let raw_body = raw_body.ok_or(PrepareConversionError::MissingRawBody)?;
    let vendor_wire_before =
        vendor_wire_before.ok_or(PrepareConversionError::MissingVendorWireBefore)?;

    let mut prepared = engine
        .prepare_request_with_patch(
            selection.profile,
            raw_body,
            selection.patch,
            selection.identity,
        )
        .await?;
    let vendor_patch =
        value_patch(vendor_wire_before, &native_body).map_err(CompatError::InvalidRequestJson)?;
    if let Some(patch) = vendor_patch {
        prepared.body = engine.apply_json_patch(prepared.body, patch)?;
    }

    Ok(PreparedConversion {
        plan,
        body: PreparedBody::Raw(prepared.body),
        force_upstream_stream: prepared.force_upstream_stream,
        session: PreparedSession::RawWireCompat(Box::new(prepared.session)),
    })
}

pub(crate) fn rebuild_raw_wire_request(
    body: PreparedBody,
    force_upstream_stream: bool,
    session: PreparedSession,
) -> Result<PreparedRequest, PrepareConversionError> {
    match (body, session) {
        (PreparedBody::Raw(body), PreparedSession::RawWireCompat(session)) => Ok(PreparedRequest {
            body,
            force_upstream_stream,
            session: *session,
        }),
        _ => Err(PrepareConversionError::StrategyStateMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyro_ccswitch_compat::{ConversionProfile, SessionIdentity};
    use serde_json::json;

    use crate::conversion::resolver::{
        RawWireCompatSelection, ResolveConversionInput, resolve_conversion,
    };
    use crate::protocol::ids::OPENAI_RESPONSES_V1;

    #[tokio::test]
    async fn prepares_passthrough_json_without_raw_session() {
        let resolved = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: None,
            protocol_is_native: true,
            request_passthrough: true,
            response_passthrough: true,
        })
        .unwrap();
        let prepared = prepare_conversion(PrepareConversionInput {
            resolved,
            engine: &CompatEngine::default(),
            native_body: json!({"model": "gpt-5"}),
            raw_body: None,
            vendor_wire_before: None,
        })
        .await
        .unwrap();

        assert!(!prepared.is_raw_wire());
        assert_eq!(prepared.plan().kind(), ConversionKind::PassThrough);
        let (_, body, _, session) = prepared.into_parts();
        assert!(matches!(body, PreparedBody::Json(_)));
        assert!(matches!(session, PreparedSession::PassThrough));
    }

    fn raw_selection() -> RawWireCompatSelection {
        RawWireCompatSelection {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            profile: ConversionProfile::xai_responses_native(false).with_model("grok-4.5"),
            identity: SessionIdentity::generated("test"),
            patch: None,
            context_1m: false,
        }
    }

    #[tokio::test]
    async fn prepares_raw_wire_bytes_and_owned_session() {
        let resolved = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: Some(raw_selection()),
            protocol_is_native: true,
            request_passthrough: false,
            response_passthrough: false,
        })
        .unwrap();
        let body = json!({"model":"grok-4.5","input":"hello"});
        let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
        let prepared = prepare_conversion(PrepareConversionInput {
            resolved,
            engine: &CompatEngine::default(),
            native_body: body.clone(),
            raw_body: Some(raw),
            vendor_wire_before: Some(&body),
        })
        .await
        .unwrap();

        assert!(prepared.is_raw_wire());
        let (_, body, force_stream, session) = prepared.into_parts();
        let request = rebuild_raw_wire_request(body, force_stream, session).unwrap();
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(value["model"], "grok-4.5");
        assert_eq!(
            request.session.profile.direction,
            nyro_ccswitch_compat::Direction::XaiResponsesNative
        );
    }

    /// 线上回归（请求 71c6c323）：路由开启 force_max_reasoning 后，compat
    /// 出站体必须带 max 档。dispatcher 的 IR 注入位于 `vendor_wire_before`
    /// 捕获之后，max 改写只能经 before→after 的 vendor 补丁上车；若把注入
    /// 点提前到捕获之前，before/after 同值、diff 为空，compat 体将保留
    /// 客户端原始档位（high）——正是该次线上症状。
    #[tokio::test]
    async fn raw_wire_vendor_patch_carries_force_max_reasoning() {
        let selection = RawWireCompatSelection {
            ingress: OPENAI_RESPONSES_V1,
            egress: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            profile: ConversionProfile::codex_responses_to_chat(false),
            identity: SessionIdentity::generated("test"),
            patch: None,
            context_1m: false,
        };
        let resolved = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            raw_wire: Some(selection),
            protocol_is_native: false,
            request_passthrough: false,
            response_passthrough: false,
        })
        .unwrap();
        let client_body =
            json!({"model":"glm-5.3-flash","input":"hello","reasoning":{"effort":"high"}});
        let raw = Bytes::from(serde_json::to_vec(&client_body).unwrap());
        // before = 未注入 force-max 的编码体；after = 注入后的编码体。
        // 补丁 diff 只含 reasoning_effort: high → max，正是要上车的东西。
        let before = json!({
            "model":"glm-5.3-flash",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false,
            "thinking":{"type":"enabled"},
            "reasoning_effort":"high"
        });
        let after = json!({
            "model":"glm-5.3-flash",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false,
            "thinking":{"type":"enabled"},
            "reasoning_effort":"max"
        });
        let prepared = prepare_conversion(PrepareConversionInput {
            resolved,
            engine: &CompatEngine::default(),
            native_body: after,
            raw_body: Some(raw),
            vendor_wire_before: Some(&before),
        })
        .await
        .unwrap();

        assert!(prepared.is_raw_wire());
        let (_, body, force_stream, session) = prepared.into_parts();
        let request = rebuild_raw_wire_request(body, force_stream, session).unwrap();
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            value["reasoning_effort"], "max",
            "force-max rewrite must ride the vendor patch onto the compat body",
        );
    }

    #[tokio::test]
    async fn raw_wire_requires_exact_raw_body() {
        let resolved = resolve_conversion(ResolveConversionInput {
            ingress: OPENAI_RESPONSES_V1,
            egress: OPENAI_RESPONSES_V1,
            raw_wire: Some(raw_selection()),
            protocol_is_native: true,
            request_passthrough: false,
            response_passthrough: false,
        })
        .unwrap();
        let body = json!({"model":"grok-4.5","input":"hello"});
        let error = prepare_conversion(PrepareConversionInput {
            resolved,
            engine: &CompatEngine::default(),
            native_body: body.clone(),
            raw_body: None,
            vendor_wire_before: Some(&body),
        })
        .await
        .unwrap_err();
        assert!(matches!(error, PrepareConversionError::MissingRawBody));
    }
}
