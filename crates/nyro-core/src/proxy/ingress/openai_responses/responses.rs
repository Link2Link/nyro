//! Thin ingress shell: POST /v1/responses

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::Gateway;
use crate::protocol::ids::OPENAI_RESPONSES_V1;
use crate::protocol::ir::RawEnvelope;
use crate::proxy::context::RequestContext;
use crate::proxy::dispatcher::{dispatch_pipeline, log_decode_error};
use crate::proxy::intake::JsonIntake;

pub async fn handler(
    State(gw): State<Gateway>,
    mut ctx: axum::extract::Extension<RequestContext>,
    headers: HeaderMap,
    intake: JsonIntake,
) -> Response {
    ctx.ingress_protocol = OPENAI_RESPONSES_V1;
    let JsonIntake { value: body, raw } = intake;
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(Some(body.clone()), flat_headers, "POST", "/v1/responses")
        .with_raw_body(raw);
    let decoder = OPENAI_RESPONSES_V1.handler().make_request_decoder();
    let request = match decoder.decode_request(body) {
        Ok(r) => r,
        Err(e) => return log_decode_error(&gw, &envelope, OPENAI_RESPONSES_V1, e),
    };
    dispatch_pipeline(gw, headers, envelope, request, OPENAI_RESPONSES_V1, ctx.0).await
}
