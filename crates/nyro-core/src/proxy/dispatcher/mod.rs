//! Dispatcher: single orchestration point that drives a request through the
//! full proxy pipeline.
//!
//! `dispatch_pipeline` is the canonical entry point. Each ingress thin-shell
//! decodes the incoming body into an `InternalRequest` and calls this function.
//!
//! Pipeline:
//!   1. Route lookup + type gate (embedding vs chat).
//!   2. `authorize_route_access` (API-key auth + quota).
//!   3. Request hooks.
//!   4. Target iteration (health-aware): for each live target →
//!      a. Resolve `Provider` + `ProviderRuntime`.
//!      b. Resolve egress protocol + base URL via `negotiate()`.
//!      c. Look up `Vendor` from `VendorRegistry`.
//!      d. Build outbound: `ProtocolMode::Native` + no mutations → `passthrough_run`;
//!      else full 7-step `adapter.build_request`.
//!      e. Merge `runtime_binding` extra-headers.
//!      f. HTTP call → `handle_non_stream` / `handle_stream`.
//!      g. On success: record health, return; on retryable error: continue.
//!   5. Return last error or 502.

mod accumulator;
mod auth;
mod compat;
mod non_stream;
mod param_overrides;
mod stream;
mod util;
use self::accumulator::*;
use self::auth::{GatewayProxyAccessStore, authorize_model_access, get_provider};
use self::non_stream::{handle_non_stream, handle_non_stream_via_upstream_stream};
use self::stream::handle_stream;
use self::util::*;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::Gateway;
use crate::db::models::Provider;
use crate::error::{AuthFailure, GatewayError};
use crate::plugin::phase::{
    HostContext, Phase, PhaseCtx, PhaseHook, PhaseHookRegistry, PhaseOutcome, ResponseStats,
    ResponseView,
};
use crate::protocol::ProviderProtocols;
use crate::protocol::codec::tool_bridge::ToolRoutePlan;
use crate::protocol::ids::ProtocolId;
use crate::protocol::ir::Usage;
use crate::protocol::ir::{AiRequest, AiResponse, RawEnvelope};
use crate::provider::VendorRegistry;
use crate::provider::vendor::ProviderCtx;
use crate::proxy::client::ProxyClient;
use crate::proxy::context::{ContextBag, RequestContext};
use crate::proxy::observability::{LogExtras, send_log};
use crate::proxy::planner::{ProtocolMode, negotiate};
use crate::router::TargetSelector;
use crate::router::health::HealthPermit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HealthOutcome {
    Success,
    Failure,
    Neutral,
    Deferred,
}

pub(super) fn health_outcome_from_status(status: u16) -> HealthOutcome {
    if status < 400 {
        HealthOutcome::Success
    } else if is_health_failure(status) {
        HealthOutcome::Failure
    } else {
        HealthOutcome::Neutral
    }
}

#[derive(Clone, Copy, Debug)]
struct LocalHealthNeutral;

fn health_outcome_from_response(response: &Response) -> HealthOutcome {
    if response.extensions().get::<LocalHealthNeutral>().is_some() {
        HealthOutcome::Neutral
    } else {
        health_outcome_from_status(response.status().as_u16())
    }
}

fn defer_stream_health(
    response: Response,
    ingress: ProtocolId,
    req_ctx: &RequestContext,
    health_permit: HealthPermit,
) -> Response {
    let (parts, body) = response.into_parts();
    let mut body = body.into_data_stream();
    let mut parser = ingress.handler().make_stream_response_decoder();
    let deadline = req_ctx.deadline.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(async move {
        let mut completed = false;
        let mut failed = false;
        loop {
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    if failed {
                        health_permit.failure();
                    } else {
                        health_permit.neutral();
                    }
                    return;
                }
                _ = tokio::time::sleep(deadline.remaining()) => {
                    health_permit.failure();
                    return;
                }
                item = body.next() => item,
            };
            let Some(item) = item else {
                break;
            };
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => {
                    health_permit.failure();
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            };
            match parser.parse_chunk(&String::from_utf8_lossy(&bytes)) {
                Ok(deltas) => update_stream_health_state(&deltas, &mut completed, &mut failed),
                Err(_) => failed = true,
            }
            tokio::select! {
                biased;
                _ = tx.closed() => {
                    if failed {
                        health_permit.failure();
                    } else {
                        health_permit.neutral();
                    }
                    return;
                }
                _ = tokio::time::sleep(deadline.remaining()) => {
                    if failed {
                        health_permit.failure();
                    } else {
                        health_permit.neutral();
                    }
                    return;
                }
                result = tx.send(Ok(bytes)) => {
                    if result.is_err() {
                        if failed {
                            health_permit.failure();
                        } else {
                            health_permit.neutral();
                        }
                        return;
                    }
                }
            }
        }
        match parser.finish() {
            Ok(deltas) => update_stream_health_state(&deltas, &mut completed, &mut failed),
            Err(_) => failed = true,
        }
        if completed && !failed {
            health_permit.success();
        } else {
            health_permit.failure();
        }
    });

    Response::from_parts(parts, Body::from_stream(ReceiverStream::new(rx)))
}

fn update_stream_health_state(
    deltas: &[crate::protocol::ir::AiStreamDelta],
    completed: &mut bool,
    failed: &mut bool,
) {
    for delta in deltas {
        match delta {
            crate::protocol::ir::AiStreamDelta::Done { stop_reason }
                if !stop_reason.eq_ignore_ascii_case("error") =>
            {
                *completed = true;
            }
            crate::protocol::ir::AiStreamDelta::Done { .. }
            | crate::protocol::ir::AiStreamDelta::StreamError { .. }
            | crate::protocol::ir::AiStreamDelta::UnexpectedEof => *failed = true,
            _ => {}
        }
    }
}

// ── Phase hook dispatch (lifecycle RFC P1-c) ────────────────────────────────────

/// Run every registered [`crate::plugin::phase::PhaseHook`] for `phase` in
/// deterministic order, threading the shared [`PhaseCtx`]. Returns the first
/// non-`Continue` outcome (short-circuit / reject), or `Continue` when all hooks
/// pass or none are registered.
///
/// Zero-overhead no-op when no phase hooks are registered, which is the default
/// in production builds — so inserting these call sites is behaviour-neutral
/// until a plugin opts in.
async fn run_phase_hooks(
    phase: Phase,
    req_ctx: &mut RequestContext,
    request: &mut AiRequest,
    response: ResponseView<'_>,
    host: &HostContext<'_>,
) -> PhaseOutcome {
    let registry = PhaseHookRegistry::global();
    if registry.all().is_empty() {
        return PhaseOutcome::Continue;
    }
    let hooks = registry.for_phase(phase);
    let outcome = run_phase_hooks_slice(&hooks, req_ctx, request, response, host).await;
    normalize_phase_outcome(phase, outcome)
}

fn normalize_phase_outcome(phase: Phase, outcome: PhaseOutcome) -> PhaseOutcome {
    if phase != Phase::OnResponse {
        return outcome;
    }
    match outcome {
        PhaseOutcome::ShortCircuit(mut response) => {
            if is_health_failure(response.status().as_u16()) {
                response.extensions_mut().insert(LocalHealthNeutral);
            }
            PhaseOutcome::ShortCircuit(response)
        }
        PhaseOutcome::Reject(error) => {
            let mut response = error.render(None);
            if is_health_failure(response.status().as_u16()) {
                response.extensions_mut().insert(LocalHealthNeutral);
            }
            PhaseOutcome::ShortCircuit(response)
        }
        PhaseOutcome::Continue => PhaseOutcome::Continue,
    }
}

/// Run a precomputed list of phase hooks against one [`PhaseCtx`].
///
/// Used by the streaming `OnResponse` path, which resolves the hook list once
/// and re-invokes it per [`crate::protocol::ir::AiStreamDelta`] from inside a
/// spawned task — avoiding a registry query (and its allocation) per chunk.
async fn run_phase_hooks_slice(
    hooks: &[&Arc<dyn PhaseHook>],
    req_ctx: &mut RequestContext,
    request: &mut AiRequest,
    response: ResponseView<'_>,
    host: &HostContext<'_>,
) -> PhaseOutcome {
    if hooks.is_empty() {
        return PhaseOutcome::Continue;
    }
    let mut pctx = PhaseCtx {
        req_ctx,
        request,
        response,
        host,
    };
    for hook in hooks {
        match hook.run(&mut pctx).await {
            PhaseOutcome::Continue => {}
            outcome => return outcome,
        }
    }
    PhaseOutcome::Continue
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Full pipeline entry point.
///
/// Each ingress shell captures the raw body in a `RawEnvelope` and decodes
/// the body into an `AiRequest`, then hands off here.
///
/// Pipeline:
///   a. Resolve egress protocol + base URL via `negotiate()`.
///   b. Auth.
///   c. Look up `Vendor` from `VendorRegistry`.
///   d. Build outbound: `ProtocolMode::Native` + no mutations → `passthrough_run`;
///      else full 7-step `adapter.build_request`.
///   e. HTTP call → `handle_non_stream` / `handle_stream`.
pub async fn dispatch_pipeline(
    gw: Gateway,
    headers: HeaderMap,
    envelope: RawEnvelope,
    request: AiRequest,
    ingress: ProtocolId,
    mut ctx: RequestContext,
) -> Response {
    // Stable host boundary; created here so the terminal OnLog phase can borrow
    // it after the core pipeline (which owns a clone of `gw`) returns.
    let host = HostContext::new(&gw);
    let mut request = request;
    let response = dispatch_pipeline_inner(
        gw.clone(),
        headers,
        envelope,
        &mut request,
        ingress,
        &mut ctx,
        &host,
    )
    .await;

    // ── OnLog phase ────────────────────────────────────────────────────────────
    // Terminal, fire-and-forget: the client response is already materialised, so
    // hooks observe (never mutate / short-circuit) the canonical `ResponseStats`
    // snapshot in `ctx.extensions`. No-op when no OnLog hooks are registered, so
    // this call site is behaviour-neutral until a plugin opts in.
    let _ = run_phase_hooks(
        Phase::OnLog,
        &mut ctx,
        &mut request,
        ResponseView::Pending,
        &host,
    )
    .await;
    response
}

async fn dispatch_pipeline_inner(
    gw: Gateway,
    headers: HeaderMap,
    envelope: RawEnvelope,
    request: &mut AiRequest,
    ingress: ProtocolId,
    ctx: &mut RequestContext,
    host: &HostContext<'_>,
) -> Response {
    // Derive logging strings from envelope.
    let method_owned = envelope.method.clone();
    let path_owned = envelope.path.clone();
    let request_body_str = envelope
        .body
        .as_ref()
        .and_then(|b| serde_json::to_string(b).ok());
    let raw_body = envelope.raw_body.clone().or_else(|| {
        envelope
            .body
            .as_ref()
            .and_then(|body| serde_json::to_vec(body).ok())
            .map(Bytes::from)
    });
    let baseline_request = request.clone();
    let request_headers_str =
        crate::proxy::observability::header_map_to_redacted_json(&envelope.headers);
    // Built early so it can be used by both pre-loop log entries and the per-target handlers.
    let req_extras = RequestExtras {
        method: method_owned.clone(),
        path: path_owned.clone(),
        headers: request_headers_str.clone(),
        body: request_body_str.clone(),
    };
    let start = Instant::now();
    // Snapshot the client-requested reasoning effort from the normalized IR
    // before hooks or provider-specific processing can mutate the request.
    // This metadata is logged independently of payload recording.
    let reasoning_effort = crate::protocol::codec::reasoning::effort_snapshot(&request.reasoning);

    // ── OnRequest phase ──────────────────────────────────────────────────────
    // Hooks run before the routing key is derived, so they may reshape the
    // request (e.g. rewrite `request.model`) before route lookup / auth.
    // Shared request-scoped extension bag, captured before the per-target
    // `ProviderCtx` shadows `ctx`; handlers write `ResponseStats` into it.
    let req_ctx_ext = ctx.extensions.clone();
    match run_phase_hooks(Phase::OnRequest, ctx, request, ResponseView::Pending, host).await {
        PhaseOutcome::Continue => {}
        PhaseOutcome::ShortCircuit(resp) => return resp,
        PhaseOutcome::Reject(e) => return e.render(None),
    }

    let request_model = request.model.clone();
    let is_stream = request.stream.enabled;
    let ingress_str = ingress.to_string();

    // ── Route lookup ─────────────────────────────────────────────────────────

    let route = {
        let cache = gw.model_cache.read().await;
        cache.match_model(&request_model).cloned()
    };
    let route = match route {
        Some(r) => r,
        None => {
            let msg = format!("no route for model: {request_model}");
            LogBuilder::from_dispatch(&gw, &ingress_str, &request_model, None, start)
                .stream_flag(is_stream)
                .reasoning_effort(reasoning_effort.clone())
                .status(404)
                .with_req_extras(&req_extras)
                .resp_body(Some(
                    serde_json::json!({ "error": { "message": msg.clone() } }).to_string(),
                ))
                .emit();
            return error_response(404, &msg);
        }
    };

    // ── Auth ─────────────────────────────────────────────────────────────────

    let access_store = GatewayProxyAccessStore::new(&gw);
    let auth_key = match authorize_model_access(
        &access_store,
        &route,
        &headers,
        gw.config.auth_key.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => {
            let status = resp.status().as_u16() as i32;
            LogBuilder::from_dispatch(&gw, &ingress_str, &request_model, None, start)
                .stream_flag(is_stream)
                .reasoning_effort(reasoning_effort.clone())
                .status_i32(status)
                .with_req_extras(&req_extras)
                .emit();
            return resp;
        }
    };

    // ── Request hooks ──────────────────────────────────────────────────────────

    let hook_registry = crate::integrations::HookRegistry::global();
    if hook_registry.has_request_hooks() {
        let hook_ctx = crate::integrations::HookContext {
            model_id: route.id.clone(),
            provider_name: String::new(),
            model: request.model.clone(),
            api_key_id: auth_key.id.clone(),
        };
        for hook in hook_registry.request_hooks() {
            if let Err(e) = hook.on_request(&hook_ctx, request).await {
                tracing::warn!(hook = hook.name(), error = %e, "request hook rejected request");
                LogBuilder::from_dispatch(
                    &gw,
                    &ingress_str,
                    &request_model,
                    auth_key.id.as_deref(),
                    start,
                )
                .stream_flag(is_stream)
                .reasoning_effort(reasoning_effort.clone())
                .status(500)
                .with_req_extras(&req_extras)
                .emit();
                return error_response(500, &e.to_string());
            }
        }
    }

    // ── OnAccess phase ───────────────────────────────────────────────────────
    // Identity (auth_key) and route are resolved; hooks may enforce access
    // policy and reject the request before any upstream work begins.
    match run_phase_hooks(Phase::OnAccess, ctx, request, ResponseView::Pending, host).await {
        PhaseOutcome::Continue => {}
        PhaseOutcome::ShortCircuit(resp) => return resp,
        PhaseOutcome::Reject(e) => {
            let resp = e.render(None);
            let status = resp.status().as_u16() as i32;
            LogBuilder::from_dispatch(
                &gw,
                &ingress_str,
                &request_model,
                auth_key.id.as_deref(),
                start,
            )
            .stream_flag(is_stream)
            .reasoning_effort(reasoning_effort.clone())
            .status_i32(status)
            .with_req_extras(&req_extras)
            .emit();
            return resp;
        }
    }

    // ── Target iteration ──────────────────────────────────────────────────────

    let targets = load_model_backends(&gw, &route).await;
    if targets.is_empty() {
        LogBuilder::from_dispatch(
            &gw,
            &ingress_str,
            &request_model,
            auth_key.id.as_deref(),
            start,
        )
        .stream_flag(is_stream)
        .reasoning_effort(reasoning_effort.clone())
        .status(503)
        .with_req_extras(&req_extras)
        .emit();
        return error_response(503, "no route targets configured");
    }
    let ordered_targets = TargetSelector::select_ordered(&route.balance, &targets);
    if ordered_targets.is_empty() {
        LogBuilder::from_dispatch(
            &gw,
            &ingress_str,
            &request_model,
            auth_key.id.as_deref(),
            start,
        )
        .stream_flag(is_stream)
        .reasoning_effort(reasoning_effort.clone())
        .status(503)
        .with_req_extras(&req_extras)
        .emit();
        return error_response(503, "no route targets configured");
    }

    let target_count = ordered_targets.len();
    let mut quota_skipped = 0_usize;
    let mut last_response: Option<Response> = None;
    for target in ordered_targets {
        if !gw.quota_registry.is_schedulable(&target.provider_id) {
            quota_skipped += 1;
            continue;
        }
        let provider = match get_provider(&access_store, &target.provider_id).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let actual_model = if target.model.is_empty() || target.model == "*" {
            request_model.clone()
        } else {
            target.model.clone()
        };

        let mut request_for_target = request.clone();

        // Resolve egress protocol + base URL via negotiate().
        // The request-scoped `ctx` is threaded end-to-end from the ingress
        // middleware (no per-target throwaway context); negotiate records its
        // trace/egress decision onto it.
        let provider_protocols = ProviderProtocols::from_provider(&provider);
        let plan = match negotiate(ingress, None, Some(&provider_protocols), ctx) {
            Ok(p) => p,
            Err(e) => {
                last_response = Some(e.render(None));
                continue;
            }
        };
        let egress = plan.egress;
        let target_key = format!("{}:{}:{}", target.provider_id, egress, actual_model);
        let Some(health_permit) = gw.health_registry.try_acquire(&target_key) else {
            continue;
        };

        let mut provider_runtime = match gw.admin().resolve_provider_runtime(&provider).await {
            Ok(runtime) => runtime,
            Err(e) => {
                last_response = Some(error_response(
                    502,
                    &format!("provider credential error: {e}"),
                ));
                continue;
            }
        };
        if let Some(endpoint_id) = plan.endpoint_id.as_deref() {
            let Some(endpoint) = provider
                .protocol_endpoints
                .iter()
                .find(|endpoint| endpoint.id == endpoint_id && endpoint.is_enabled)
            else {
                last_response = Some(error_response(
                    503,
                    "adaptive provider endpoint is missing or disabled",
                ));
                continue;
            };
            provider_runtime.access_token = endpoint.api_key.clone();
            provider_runtime.binding = crate::auth::types::RuntimeBinding::default();
        }
        let egress_base_url = if let Some(base_url_override) = provider_runtime
            .binding
            .base_url_override
            .clone()
            .filter(|v| !v.trim().is_empty())
        {
            base_url_override
        } else if plan.base_url.is_empty() {
            provider.base_url.clone()
        } else {
            plan.base_url.clone()
        };

        // Look up Vendor for this vendor_id.
        let vendor_id = provider
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("custom");
        let adapter = match VendorRegistry::global().get_vendor(vendor_id) {
            Some(a) => a.clone(),
            None => {
                last_response = Some(error_response(
                    503,
                    &format!("no vendor registered for '{vendor_id}'"),
                ));
                continue;
            }
        };

        // ── OnUpstream phase ─────────────────────────────────────────────────
        // Target + vendor are selected but the upstream call has not happened.
        // Hooks may short-circuit here (e.g. cache hit) to skip the upstream.
        // Runs per-attempt inside the retry loop (see lifecycle RFC §5.1).
        match run_phase_hooks(
            Phase::OnUpstream,
            ctx,
            &mut request_for_target,
            ResponseView::Pending,
            host,
        )
        .await
        {
            PhaseOutcome::Continue => {}
            PhaseOutcome::ShortCircuit(resp) => return resp,
            PhaseOutcome::Reject(e) => {
                last_response = Some(e.render(None));
                continue;
            }
        }

        let compat_selection = if compat::supports_compat_request(
            ingress,
            egress,
            &provider,
            &egress_base_url,
            &actual_model,
        ) {
            let Some(raw_body) = raw_body.as_deref() else {
                last_response = Some(error_response(500, "compat request is missing raw body"));
                continue;
            };
            match compat::select_compat_request(
                ingress,
                egress,
                &provider,
                &egress_base_url,
                &actual_model,
                is_stream,
                &headers,
                raw_body,
                &baseline_request,
                &request_for_target,
            ) {
                Ok(selection) => selection,
                Err(error) => {
                    last_response = Some(error_response(
                        500,
                        &format!("compat request preparation failed: {error}"),
                    ));
                    continue;
                }
            }
        } else {
            None
        };
        let compat_candidate = compat_selection.is_some();
        let transport_model = compat_selection
            .as_ref()
            .and_then(|selection| selection.profile.model.as_deref())
            .unwrap_or(&actual_model);
        let mut upstream_request = request_for_target.clone();
        let tool_route_plan = if compat_candidate {
            ToolRoutePlan::default()
        } else {
            let plan = ToolRoutePlan::for_request(&upstream_request, egress);
            plan.prepare_upstream_request(&mut upstream_request);
            plan
        };

        let credential = provider_runtime.access_token.clone();
        // Vendor-level provider context for codec ops. Named distinctly so it
        // does NOT shadow the threaded `RequestContext` (`ctx`), which the
        // handlers now need for the `OnResponse` phase.
        let provider_ctx = ProviderCtx {
            provider: &provider,
            protocol: egress,
            egress_base_url: &egress_base_url,
            api_key: &credential,
            auth_scheme: &plan.auth_scheme,
            actual_model: transport_model,
            credential: None,
            gw: &gw,
            disable_default_auth: provider_runtime.binding.disable_default_auth,
        };

        let passthrough_resp = !compat_candidate
            && plan.mode == ProtocolMode::Native
            && !adapter.declared_response_mutations();
        let vendor_wire_before = if compat_candidate {
            let encoder = egress.handler().make_request_encoder();
            encoder
                .encode_request(&upstream_request)
                .ok()
                .map(|encoded| encoded.0)
        } else {
            None
        };

        // Provider/model-specific parameter rewrites. Applied after
        // `vendor_wire_before` on purpose: the compat path rebuilds the
        // upstream body from the raw client body (which still carries the
        // rejected value) and merges in the before -> after vendor patch, so
        // rewriting here makes the change ride that patch. A rewrite also
        // forces the re-encode path below, skipping the native passthrough
        // that would forward the verbatim client body.
        let param_override_applied = param_overrides::apply_upstream_param_overrides(
            &mut upstream_request,
            &provider,
            &actual_model,
        );

        // Build outbound request — PassThrough (Native + no mutations) or full 7-step pipeline.
        let passthrough_req = !compat_candidate
            && plan.mode == ProtocolMode::Native
            && !adapter.declared_request_mutations()
            && !tool_route_plan.is_active()
            && !param_override_applied;
        let mut outbound = if passthrough_req {
            let raw = envelope.body.clone().unwrap_or_default();
            match crate::provider::common::pipeline::passthrough_run(
                adapter.as_ref(),
                raw,
                &provider_ctx,
                is_stream,
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    last_response = Some(e.render(None));
                    continue;
                }
            }
        } else {
            match adapter
                .build_request(&mut upstream_request, &provider_ctx)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    last_response = Some(e.render(None));
                    continue;
                }
            }
        };

        // Channel-scoped extensions own URL quirks such as ChatGPT Codex's
        // `/backend-api/codex/responses` path. The full vendor builds the
        // request body, then the resolved extension canonicalizes its URL.
        if let Some(extension) = VendorRegistry::global().resolve(&provider, egress) {
            let vendor_ctx = provider_ctx.to_vendor_ctx();
            let egress_path = egress
                .handler()
                .make_request_encoder()
                .egress_path(transport_model, is_stream);
            outbound.url = extension.build_url(&vendor_ctx, &egress_base_url, &egress_path);
        }

        // Merge safe client headers, runtime-binding headers, and adapter
        // headers. Provider-owned identity must override client hints: OAuth
        // runtimes such as Codex and Claude rely on canonical User-Agent,
        // originator/beta, and account headers that callers must not spoof.
        //
        // Precedence: forwarded client hints < adapter < runtime binding.
        // Sensitive client headers (auth keys, cookies, IP/host forwarding
        // metadata, hop-by-hop transport headers) are filtered in
        // `forwarded_client_headers`. Runtime bindings are provider-owned and
        // remain authoritative for both OAuth auth and identity headers.
        match runtime_binding_headers(&provider_runtime.binding) {
            Ok(binding_hdrs) => {
                let mut merged = forwarded_client_headers(&headers);
                merged.extend(outbound.headers);
                merged.extend(binding_hdrs);
                outbound.headers = merged;
            }
            Err(e) => {
                last_response = Some(error_response(
                    502,
                    &format!("provider runtime binding error: {e}"),
                ));
                continue;
            }
        }

        let prepared_compat = if let Some(selection) = compat_selection.as_ref() {
            let (Some(raw_body), Some(vendor_wire_before)) =
                (raw_body.clone(), vendor_wire_before.as_ref())
            else {
                last_response = Some(error_response(
                    500,
                    "compat request is missing its raw or pre-vendor wire body",
                ));
                continue;
            };
            match compat::prepare_compat_request(
                gw.compat_engine.as_ref(),
                selection,
                raw_body,
                vendor_wire_before,
                &outbound.body,
            )
            .await
            {
                Ok(prepared) => {
                    if let Err(error) = compat::normalize_compat_request_headers(
                        &mut outbound.headers,
                        selection,
                        &req_extras.path,
                    ) {
                        last_response = Some(error_response(
                            500,
                            &format!("compat request header preparation failed: {error}"),
                        ));
                        continue;
                    }
                    Some(prepared)
                }
                Err(error) => {
                    // Invalid client history fails on every provider the same
                    // way; cc-switch classifies these NonRetryable and so do
                    // we — return to the client instead of replaying the
                    // broken request at each target. Other conversion errors
                    // carry their cc-switch status (422 for transform
                    // failures) instead of a generic 500.
                    let message = format!("compat request preparation failed: {error}");
                    let status = error.http_status();
                    if error.is_invalid_request() {
                        return error_response(status, &message);
                    }
                    last_response = Some(unprocessable_response(status, &message));
                    continue;
                }
            }
        } else {
            None
        };

        let client = match gw.http_client_for_provider(provider.use_proxy).await {
            Ok(http_client) => ProxyClient::new(http_client),
            Err(e) => {
                let msg = format!("provider transport error: {e}");
                last_response = Some(error_response(502, &msg));
                continue;
            }
        };

        let egress_str = egress.to_string();
        let egress_caps = egress.handler().capabilities();
        let upstream_forces_stream = egress_caps.force_upstream_stream;

        // ── Build per-target context structs ─────────────────────────────────
        let call_ctx = CallCtx {
            gw: gw.clone(),
            provider: &provider,
            model_id: &route.id,
            model_name: &route.name,
            egress,
            ingress,
            ingress_str: &ingress_str,
            egress_str: &egress_str,
            request_model: &request_model,
            actual_model: &actual_model,
            api_key_id: auth_key.id.as_deref(),
            api_key_name: auth_key.name.as_deref(),
            is_stream,
            enable_payload: route.enable_payload,
            reasoning_effort: reasoning_effort.clone(),
            start,
            req_ext: req_ctx_ext.clone(),
        };
        // `OnLog` runs once at the pipeline boundary (see `dispatch_pipeline`).
        // The handlers run the `OnResponse` phase: non-stream paths see a full
        // `AiResponse`, the streaming path is invoked per `AiStreamDelta`.
        let uses_compat = prepared_compat.is_some();
        let (response, compat_retryable, health_outcome) = if let Some(prepared) = prepared_compat {
            let attempt = compat::handle_compat(
                client,
                &outbound.url,
                outbound.headers,
                prepared,
                &call_ctx,
                &req_extras,
                ctx,
                &mut request_for_target,
                host,
                health_permit.clone(),
            )
            .await;
            (
                attempt.response,
                attempt.force_retry,
                attempt.health_outcome,
            )
        } else {
            let response = if is_stream {
                handle_stream(
                    client,
                    &outbound.url,
                    outbound.headers,
                    outbound.body,
                    &call_ctx,
                    &req_extras,
                    passthrough_resp,
                    tool_route_plan,
                    ctx,
                    &request_for_target,
                )
                .await
            } else if upstream_forces_stream {
                handle_non_stream_via_upstream_stream(
                    client,
                    &outbound.url,
                    outbound.headers,
                    outbound.body,
                    &call_ctx,
                    tool_route_plan,
                    ctx,
                    &mut request_for_target,
                    host,
                )
                .await
            } else {
                handle_non_stream(
                    client,
                    &outbound.url,
                    outbound.headers,
                    outbound.body,
                    &call_ctx,
                    &req_extras,
                    adapter.as_ref(),
                    &provider_ctx,
                    passthrough_resp,
                    &tool_route_plan,
                    ctx,
                    &mut request_for_target,
                    host,
                )
                .await
            };
            let health_outcome = health_outcome_from_response(&response);
            (response, false, health_outcome)
        };

        let status = response.status().as_u16();
        if status == 429 {
            crate::admin::trigger_provider_usage_refresh(gw.clone(), target.provider_id.clone());
        }
        let defer_native_stream_health =
            !uses_compat && is_stream && health_outcome == HealthOutcome::Success;
        let response = if defer_native_stream_health {
            defer_stream_health(response, ingress, ctx, health_permit.clone())
        } else {
            response
        };
        let health_outcome = if defer_native_stream_health {
            HealthOutcome::Deferred
        } else {
            health_outcome
        };
        match health_outcome {
            HealthOutcome::Success => health_permit.success(),
            HealthOutcome::Failure => health_permit.failure(),
            HealthOutcome::Neutral => health_permit.neutral(),
            HealthOutcome::Deferred => drop(health_permit),
        }
        if status < 400 {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            TargetSelector::record_selected(&route.balance, &target_key);
            TargetSelector::record_latency(&route.balance, &target_key, elapsed_ms);
            return response;
        }
        if compat_retryable || is_retryable(status) {
            last_response = Some(response);
            continue;
        }
        return response;
    }

    if target_count > 0 && quota_skipped == target_count {
        LogBuilder::from_dispatch(
            &gw,
            &ingress_str,
            &request_model,
            auth_key.id.as_deref(),
            start,
        )
        .stream_flag(is_stream)
        .reasoning_effort(reasoning_effort.clone())
        .status(503)
        .with_req_extras(&req_extras)
        .emit();
        return error_response(
            503,
            "all route providers are temporarily unavailable due to exhausted quota",
        );
    }

    last_response.unwrap_or_else(|| {
        LogBuilder::from_dispatch(
            &gw,
            &ingress_str,
            &request_model,
            auth_key.id.as_deref(),
            start,
        )
        .stream_flag(is_stream)
        .reasoning_effort(reasoning_effort.clone())
        .status(502)
        .with_req_extras(&req_extras)
        .emit();
        error_response(502, "all route targets failed")
    })
}

/// Legacy entry point: takes a raw `Value` body, wraps it in a `RawEnvelope`,
/// decodes it, and calls `dispatch_pipeline`.
pub async fn dispatch(
    gw: Gateway,
    headers: HeaderMap,
    body: Value,
    ingress: ProtocolId,
    method: &'static str,
    path: &'static str,
    ctx: &mut RequestContext,
) -> Response {
    let flat_headers: std::collections::HashMap<String, String> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|vs| (k.as_str().to_lowercase(), vs.to_string()))
        })
        .collect();
    let envelope = RawEnvelope::new(Some(body.clone()), flat_headers, method, path);

    let decoder = ingress.handler().make_request_decoder();
    let request = match decoder.decode_request(body) {
        Ok(r) => r,
        Err(e) => return log_decode_error(&gw, &envelope, ingress, e),
    };

    dispatch_pipeline(gw, headers, envelope, request, ingress, ctx.clone()).await
}

// ── Handler context types ─────────────────────────────────────────────────────

/// Core per-request dispatch context: routing identity, timing, and log
/// metadata. Shared by all three HTTP-level handlers so they no longer need
/// a long flat parameter list for the same information.
struct CallCtx<'a> {
    gw: Gateway,
    provider: &'a Provider,
    model_id: &'a str,
    model_name: &'a str,
    egress: ProtocolId,
    ingress: ProtocolId,
    ingress_str: &'a str,
    egress_str: &'a str,
    request_model: &'a str,
    actual_model: &'a str,
    api_key_id: Option<&'a str>,
    api_key_name: Option<&'a str>,
    is_stream: bool,
    enable_payload: Option<bool>,
    /// Client-requested reasoning effort snapshot (payload-independent).
    reasoning_effort: Option<String>,
    start: Instant,
    /// Shared request-scoped extension bag (clone of `RequestContext::extensions`);
    /// handlers write the canonical `ResponseStats` snapshot here.
    req_ext: ContextBag,
}

/// Owned request HTTP metadata kept for log entries. Used by the non-stream
/// and stream handlers (not the force-stream handler which omits request
/// details from its log path).
struct RequestExtras {
    method: String,
    path: String,
    headers: Option<String>,
    body: Option<String>,
}

// ── Log builder ───────────────────────────────────────────────────────────────

/// Fluent builder for `LogEntry`. Eliminates the long flat parameter list at
/// call sites.
///
/// Create via `LogBuilder::from_ctx` (inside handler functions, where a
/// `CallCtx` is available) or `LogBuilder::from_dispatch` (in
/// `dispatch_pipeline` before a provider has been selected).  Chain setter
/// methods for the per-call fields, then call `emit` to enqueue the entry.
#[derive(Clone)]
struct LogBuilder {
    gw: Gateway,
    client_protocol: String,
    upstream_protocol: String,
    client_model: String,
    upstream_model: String,
    api_key_id: Option<String>,
    api_key_name: Option<String>,
    provider_id: String,
    provider_name: String,
    model_id: Option<String>,
    model_name: Option<String>,
    is_stream: bool,
    enable_payload: Option<bool>,
    /// Client-requested reasoning effort snapshot; only a fallback for `emit`,
    /// which prefers the effort actually sent on the upstream wire.
    reasoning_effort: Option<String>,
    start: Instant,
    client_status_code: i32,
    usage: Usage,
    extras: LogExtras,
    /// Optional request-scoped bag; when set, `emit` mirrors the final metrics
    /// into a `ResponseStats` snapshot (lifecycle RFC OnResponse → ctx).
    ext: Option<ContextBag>,
}

impl LogBuilder {
    /// Build from a handler-level `CallCtx`; identity fields are pre-filled.
    fn from_ctx(call_ctx: &CallCtx<'_>) -> Self {
        Self {
            gw: call_ctx.gw.clone(),
            client_protocol: call_ctx.ingress_str.to_string(),
            upstream_protocol: call_ctx.egress_str.to_string(),
            client_model: call_ctx.request_model.to_string(),
            upstream_model: call_ctx.actual_model.to_string(),
            api_key_id: call_ctx.api_key_id.map(ToString::to_string),
            api_key_name: call_ctx.api_key_name.map(ToString::to_string),
            provider_id: call_ctx.provider.id.clone(),
            provider_name: call_ctx.provider.name.clone(),
            model_id: Some(call_ctx.model_id.to_string()),
            model_name: Some(call_ctx.model_name.to_string()),
            is_stream: call_ctx.is_stream,
            enable_payload: call_ctx.enable_payload,
            reasoning_effort: call_ctx.reasoning_effort.clone(),
            start: call_ctx.start,
            client_status_code: 200,
            usage: Usage::default(),
            extras: LogExtras::default(),
            ext: Some(call_ctx.req_ext.clone()),
        }
    }

    /// Build from dispatch-pipeline context before a provider is selected.
    /// `upstream_protocol` defaults to `client_protocol`; `upstream_model` and
    /// `provider_id` default to empty strings.
    fn from_dispatch(
        gw: &Gateway,
        ingress: &str,
        request_model: &str,
        api_key_id: Option<&str>,
        start: Instant,
    ) -> Self {
        Self {
            gw: gw.clone(),
            client_protocol: ingress.to_string(),
            upstream_protocol: ingress.to_string(),
            client_model: request_model.to_string(),
            upstream_model: String::new(),
            api_key_id: api_key_id.map(ToString::to_string),
            api_key_name: None,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: None,
            model_name: None,
            is_stream: false,
            enable_payload: None,
            reasoning_effort: None,
            start,
            client_status_code: 200,
            usage: Usage::default(),
            extras: LogExtras::default(),
            ext: None,
        }
    }

    fn stream_flag(mut self, v: bool) -> Self {
        self.is_stream = v;
        self
    }

    /// Attach the client-requested reasoning effort snapshot.
    fn reasoning_effort(mut self, v: Option<String>) -> Self {
        self.reasoning_effort = v;
        self
    }

    fn status(mut self, code: u16) -> Self {
        self.client_status_code = code as i32;
        self
    }

    fn status_i32(mut self, code: i32) -> Self {
        self.client_status_code = code;
        self
    }

    fn usage(mut self, u: Usage) -> Self {
        self.usage = u;
        self
    }

    fn error(self, _msg: impl Into<String>) -> Self {
        // Error info is embedded in response body; kept for call-site compat.
        self
    }

    fn maybe_error(self, _msg: Option<String>) -> Self {
        self
    }

    /// Pre-fill the client request-side `LogExtras` fields (method, path,
    /// headers, body) from a `RequestExtras`.
    fn with_req_extras(mut self, req: &RequestExtras) -> Self {
        self.extras.method = Some(req.method.clone());
        self.extras.path = Some(req.path.clone());
        self.extras.client_request_headers = req.headers.clone();
        self.extras.client_request_body = req.body.clone();
        self
    }

    /// Set the upstream request wire (headers + body encoded for upstream).
    fn with_upstream_request(mut self, headers: Option<String>, body: Option<String>) -> Self {
        self.extras.upstream_request_headers = headers;
        self.extras.upstream_request_body = body;
        self
    }

    fn upstream_url(mut self, url: &str) -> Self {
        self.extras.upstream_url = Some(crate::proxy::observability::redact_url_credentials(url));
        self
    }

    /// Set the upstream response wire.
    fn with_upstream_response(
        mut self,
        status: i32,
        headers: Option<String>,
        body: Option<String>,
        latency_ms: Option<i64>,
    ) -> Self {
        self.extras.upstream_status_code = Some(status);
        self.extras.upstream_response_headers = headers;
        self.extras.upstream_response_body = body;
        self.extras.latency_upstream_ms = latency_ms;
        self
    }

    fn upstream_resp_headers(mut self, h: Option<String>) -> Self {
        self.extras.upstream_response_headers = h;
        self
    }

    fn upstream_resp_body(mut self, b: Option<String>) -> Self {
        self.extras.upstream_response_body = b;
        self
    }

    fn upstream_status(mut self, code: i32) -> Self {
        self.extras.upstream_status_code = Some(code);
        self
    }

    /// Set the client response wire.
    fn with_client_response(mut self, headers: Option<String>, body: Option<String>) -> Self {
        self.extras.client_response_headers = headers;
        self.extras.client_response_body = body;
        self
    }

    fn stream_metrics(mut self, chunks: i32, first_chunk_ms: Option<i64>) -> Self {
        self.extras.stream_chunks_count = chunks;
        self.extras.stream_first_chunk_ms = first_chunk_ms;
        self
    }

    // ── Legacy shim ────────────────────────────────────────────────────────

    /// Maps `response_body` → `client_response_body`.
    fn resp_body(mut self, b: Option<String>) -> Self {
        self.extras.client_response_body = b;
        self
    }

    /// Reasoning effort as sent on the upstream wire, derived from the encoded
    /// upstream request body; falls back to the client-requested snapshot when
    /// no upstream body was recorded (early failures, payload disabled).
    fn resolve_reasoning_effort(&self) -> Option<String> {
        self.extras
            .upstream_request_body
            .as_deref()
            .and_then(crate::proxy::observability::upstream_reasoning_effort)
            .or_else(|| self.reasoning_effort.clone())
    }

    fn emit(self) {
        use crate::logging::LogEntry;
        let latency_total_ms = self.start.elapsed().as_millis() as i64;
        let reasoning_effort = self.resolve_reasoning_effort();
        // OnResponse → ctx: mirror the final metrics into a single canonical
        // snapshot so OnLog (and OnLogHook) read consistent values.
        if let Some(ext) = &self.ext {
            ext.insert(ResponseStats {
                client_status: self.client_status_code.max(0) as u16,
                upstream_status: self.extras.upstream_status_code.map(|c| c.max(0) as u16),
                usage: self.usage.clone(),
                upstream_latency_ms: self.extras.latency_upstream_ms,
                ttfb_ms: self.extras.stream_first_chunk_ms,
                stream_chunks: self.extras.stream_chunks_count.max(0) as u32,
            });
        }
        let entry = LogEntry {
            api_key_id: self.api_key_id,
            api_key_name: self.api_key_name,
            created_at: chrono::Utc::now().timestamp_millis(),
            client_protocol: self.client_protocol,
            upstream_protocol: self.upstream_protocol,
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            model_id: self.model_id,
            model_name: self.model_name,
            upstream_url: self.extras.upstream_url,
            client_model: self.client_model,
            upstream_model: self.upstream_model,
            reasoning_effort,
            method: self.extras.method,
            path: self.extras.path,
            client_request_headers: self.extras.client_request_headers,
            client_request_body: self.extras.client_request_body,
            client_response_headers: self.extras.client_response_headers,
            client_response_body: self.extras.client_response_body,
            upstream_request_headers: self.extras.upstream_request_headers,
            upstream_request_body: self.extras.upstream_request_body,
            upstream_response_headers: self.extras.upstream_response_headers,
            upstream_response_body: self.extras.upstream_response_body,
            upstream_status_code: self.extras.upstream_status_code,
            client_status_code: self.client_status_code,
            latency_total_ms,
            latency_upstream_ms: self.extras.latency_upstream_ms,
            usage: self.usage,
            is_stream: self.is_stream,
            stream_chunks_count: self.extras.stream_chunks_count,
            stream_first_chunk_ms: self.extras.stream_first_chunk_ms,
            enable_payload: self.enable_payload,
        };
        send_log(&self.gw, entry);
    }
}

// ── Non-streaming / streaming handlers: see non_stream.rs and stream.rs ───────
// ── Auth helpers: see auth.rs ─────────────────────────────────────────────

// Utility helpers (is_retryable, runtime_binding_headers, load_model_backends,
// forwarded_client_headers) are in util.rs.

fn ai_response_to_deltas(resp: &AiResponse) -> Vec<crate::protocol::ir::AiStreamDelta> {
    use crate::protocol::ir::AiStreamDelta;
    use crate::protocol::ir::response::ResponseItem;
    let mut deltas = vec![AiStreamDelta::MessageStart {
        id: if resp.id.is_empty() {
            format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
        } else {
            resp.id.clone()
        },
        model: resp.model.clone(),
    }];
    if let Some(reasoning) = &resp.reasoning_content
        && !reasoning.is_empty()
    {
        deltas.push(AiStreamDelta::ThinkingDelta(reasoning.clone()));
        if let Some(sig) = resp.reasoning_signature.as_ref().filter(|s| !s.is_empty()) {
            deltas.push(AiStreamDelta::ThinkingSignature(sig.clone()));
        }
    }

    if let Some(items) = &resp.items {
        let mut tool_index = 0;
        for item in items {
            match item {
                ResponseItem::OutputText { text } if !text.is_empty() => {
                    deltas.push(AiStreamDelta::TextDelta(text.clone()));
                }
                ResponseItem::Thinking { text } if !text.is_empty() => {
                    deltas.push(AiStreamDelta::ThinkingDelta(text.clone()));
                }
                ResponseItem::FunctionCall {
                    call_id,
                    name,
                    namespace,
                    arguments,
                } => {
                    deltas.push(AiStreamDelta::ToolCallStart {
                        index: tool_index,
                        id: call_id.clone(),
                        name: name.clone(),
                        namespace: namespace.clone(),
                        kind: crate::protocol::ir::ToolCallKind::Function,
                    });
                    if !arguments.is_empty() {
                        deltas.push(AiStreamDelta::ToolCallDelta {
                            index: tool_index,
                            arguments: arguments.clone(),
                        });
                    }
                    tool_index += 1;
                }
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    namespace,
                    input,
                } => {
                    deltas.push(AiStreamDelta::ToolCallStart {
                        index: tool_index,
                        id: call_id.clone(),
                        name: name.clone(),
                        namespace: namespace.clone(),
                        kind: crate::protocol::ir::ToolCallKind::Custom,
                    });
                    if !input.is_empty() {
                        deltas.push(AiStreamDelta::ToolCallDelta {
                            index: tool_index,
                            arguments: input.clone(),
                        });
                    }
                    tool_index += 1;
                }
                ResponseItem::Unknown { raw } => {
                    deltas.push(AiStreamDelta::Unknown {
                        raw: raw.to_string(),
                    });
                }
                _ => {}
            }
        }
    } else {
        if !resp.content.is_empty() {
            deltas.push(AiStreamDelta::TextDelta(resp.content.clone()));
        }
        for (index, tool_call) in resp.tool_calls.iter().enumerate() {
            deltas.push(AiStreamDelta::ToolCallStart {
                index,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                namespace: tool_call.namespace.clone(),
                kind: tool_call.kind,
            });
            if !tool_call.arguments.is_empty() {
                deltas.push(AiStreamDelta::ToolCallDelta {
                    index,
                    arguments: tool_call.arguments.clone(),
                });
            }
        }
    }

    if let Some(metadata) = resp.vendor.ingress.get("__google_response_metadata") {
        deltas.push(AiStreamDelta::Unknown {
            raw: serde_json::json!({"__google_response_metadata": metadata}).to_string(),
        });
    }
    deltas.push(AiStreamDelta::Usage(resp.usage.clone()));
    deltas.push(AiStreamDelta::Done {
        stop_reason: resp
            .stop_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string()),
    });
    deltas
}

/// Emit a `LogEntry` for a request that failed to decode at the ingress
/// boundary (before `dispatch_pipeline` runs) and return the corresponding
/// 400 `Response`. Ensures decode failures show up in the in-app log module
/// rather than only in stdout tracing.
pub(crate) fn log_decode_error(
    gw: &Gateway,
    envelope: &RawEnvelope,
    ingress: ProtocolId,
    err: impl std::fmt::Display,
) -> Response {
    let msg = format!("invalid request: {err}");
    let request_body_str = envelope
        .body
        .as_ref()
        .and_then(|b| serde_json::to_string(b).ok());
    let request_headers_str = serde_json::to_string(&envelope.headers).ok();
    let ingress_str = ingress.to_string();
    // The decoder never ran, so `request.stream.enabled` is unavailable; sniff
    // the raw body so the log's `Stream` line reflects what the client asked.
    let is_stream = envelope
        .body
        .as_ref()
        .and_then(|b| b.get("stream"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    LogBuilder::from_dispatch(gw, &ingress_str, "", None, Instant::now())
        .status(400)
        .stream_flag(is_stream)
        .with_req_extras(&RequestExtras {
            method: envelope.method.clone(),
            path: envelope.path.clone(),
            headers: request_headers_str,
            body: request_body_str,
        })
        .resp_body(Some(
            serde_json::json!({ "error": { "message": msg.clone() } }).to_string(),
        ))
        .emit();
    error_response(400, &msg)
}

pub(crate) fn error_response(status: u16, message: &str) -> Response {
    let err: GatewayError = match status {
        400 => GatewayError::bad_request("bad_request", message),
        401 => GatewayError::Unauthorized {
            reason: AuthFailure::Invalid,
        },
        403 => GatewayError::Forbidden {
            reason: crate::error::AccessDenial::Custom(message.to_string()),
        },
        404 => GatewayError::ModelNotFound {
            model: message.to_string(),
        },
        429 => GatewayError::QuotaExceeded {
            window: crate::error::QuotaWindow {
                window_type: "request".to_string(),
                reset_at_secs: None,
            },
        },
        503 => GatewayError::provider_unavailable("unknown", message),
        502 => GatewayError::upstream_status("unknown", 502, Some(message.to_string())),
        _ => GatewayError::Internal {
            source: anyhow::anyhow!("{}", message),
        },
    };
    let mut response = err.render(None);
    if status == 500 {
        response.extensions_mut().insert(LocalHealthNeutral);
    }
    response
}

// StreamResponseAccumulator and ensure_tool_index are in accumulator.rs.

/// Renders a compat conversion failure with its own HTTP status (typically
/// 422) in the gateway's standard error envelope. `error_response` has no
/// 422 arm, and routing these through `GatewayError::Internal` would mask
/// the real status as 500.
fn unprocessable_response(status: u16, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {"code": status, "message": message, "type": "NYRO_UNPROCESSABLE_ENTITY"}
    });
    Response::builder()
        .status(
            axum::http::StatusCode::from_u16(status)
                .unwrap_or(axum::http::StatusCode::UNPROCESSABLE_ENTITY),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("static response build")
}

#[cfg(test)]
mod tests {
    use super::{dispatch_pipeline, run_phase_hooks_slice};
    use crate::Gateway;
    use crate::db::models::{CreateModel, CreateModelBackend, CreateProvider};
    use crate::plugin::phase::{
        HostContext, Phase, PhaseCtx, PhaseHook, PhaseHookRegistration, PhaseOutcome, ResponseView,
    };
    use crate::protocol::ids::OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1;
    use crate::protocol::ir::{AiRequest, AiResponse, RawEnvelope};
    use crate::router::quota::QuotaTierObservation;
    use async_trait::async_trait;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── Example PhaseHook (validates the P1 lifecycle wiring end-to-end) ──────
    //
    // This hook is registered ONLY in `#[cfg(test)]` builds, so it has zero
    // effect on production or on integration-test binaries (which link the lib
    // without its test cfg). It serves as a copy-paste template for real hooks:
    // implement `PhaseHook`, then `inventory::submit!` a registration.
    //
    // Behaviour: when a request targets the sentinel model, the OnRequest hook
    // short-circuits with 200 *before* route lookup — proving the hook runs at
    // the head of the pipeline and that `PhaseOutcome::ShortCircuit` is honoured.
    // For every other model it returns `Continue`, leaving existing behaviour
    // (and the other tests in this module) untouched.

    const SENTINEL_MODEL: &str = "__nyro_onrequest_shortcircuit__";

    struct SentinelShortCircuitHook;

    #[async_trait]
    impl PhaseHook for SentinelShortCircuitHook {
        fn name(&self) -> &'static str {
            "test-onrequest-shortcircuit"
        }
        fn phase(&self) -> Phase {
            Phase::OnRequest
        }
        async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome {
            if ctx.request.model == SENTINEL_MODEL {
                PhaseOutcome::ShortCircuit(
                    (StatusCode::OK, "short-circuited by phase hook").into_response(),
                )
            } else {
                PhaseOutcome::Continue
            }
        }
    }

    inventory::submit! {
        PhaseHookRegistration { make: || std::sync::Arc::new(SentinelShortCircuitHook) }
    }

    // ── Example OnLog hook (validates the terminal phase fires) ──────────────
    //
    // Records (test-only) that the OnLog phase ran for a sentinel-model request.
    // OnLog is terminal and fire-and-forget, so the hook only observes and always
    // returns `Continue`; it leaves other tests untouched.
    const ONLOG_PROBE_MODEL: &str = "__nyro_onlog_probe__";
    static ONLOG_RAN: AtomicBool = AtomicBool::new(false);

    struct OnLogProbeHook;

    #[async_trait]
    impl PhaseHook for OnLogProbeHook {
        fn name(&self) -> &'static str {
            "test-onlog-probe"
        }
        fn phase(&self) -> Phase {
            Phase::OnLog
        }
        async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome {
            if ctx.request.model == ONLOG_PROBE_MODEL {
                ONLOG_RAN.store(true, Ordering::SeqCst);
            }
            PhaseOutcome::Continue
        }
    }

    inventory::submit! {
        PhaseHookRegistration { make: || std::sync::Arc::new(OnLogProbeHook) }
    }

    #[tokio::test]
    async fn dispatch_logs_client_request_headers_redacted_when_route_missing() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-client-header-redaction-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, mut log_rx) = Gateway::new(config).await.expect("gateway init");
        let mut envelope_headers = HashMap::new();
        envelope_headers.insert("authorization".into(), "Bearer client-secret".into());
        envelope_headers.insert("x-api-key".into(), "client-key".into());
        envelope_headers.insert("content-type".into(), "application/json".into());
        let envelope = RawEnvelope::new(
            Some(serde_json::json!({"model": "missing-model"})),
            envelope_headers,
            "POST",
            "/v1/chat/completions",
        );
        let request = AiRequest::new("missing-model", Vec::new());

        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("log entry should be emitted")
            .expect("log channel should remain open");
        let headers = entry
            .client_request_headers
            .as_deref()
            .expect("client headers should be logged");
        let parsed: Value = serde_json::from_str(headers).expect("headers should be JSON");
        assert_eq!(parsed["authorization"], "***");
        assert_eq!(parsed["x-api-key"], "***");
        assert_eq!(parsed["content-type"], "application/json");
        assert!(!headers.contains("client-secret"));
        assert!(!headers.contains("client-key"));
    }

    #[tokio::test]
    async fn on_request_phase_hook_short_circuits_before_route_lookup() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-onrequest-hook-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let envelope = RawEnvelope::new(
            Some(serde_json::json!({ "model": SENTINEL_MODEL })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        );
        let request = AiRequest::new(SENTINEL_MODEL, Vec::new());

        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        // No route is configured for the sentinel model — a normal request would
        // 404 at route lookup. A 200 here proves the OnRequest hook ran first and
        // its ShortCircuit response was returned through the real pipeline.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn on_log_phase_hook_runs_at_pipeline_end() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir()
                .join(format!("nyro-onlog-hook-test-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let envelope = RawEnvelope::new(
            Some(serde_json::json!({ "model": ONLOG_PROBE_MODEL })),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        );
        let request = AiRequest::new(ONLOG_PROBE_MODEL, Vec::new());

        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        // No route for the probe model → 404, but OnLog is terminal and must fire
        // unconditionally after the core pipeline returns.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            ONLOG_RAN.load(Ordering::SeqCst),
            "OnLog phase hook should run at the pipeline boundary"
        );
    }

    // OnResponse (full body) hook used to validate that the Full view is mutable
    // and that the outcome is honoured. Not inventory-registered — driven
    // directly via `run_phase_hooks_slice` so it never affects the live pipeline.
    struct FullMutateHook;

    #[async_trait]
    impl PhaseHook for FullMutateHook {
        fn name(&self) -> &'static str {
            "test-onresponse-full-mutate"
        }
        fn phase(&self) -> Phase {
            Phase::OnResponse
        }
        async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome {
            if let ResponseView::Full(resp) = &mut ctx.response {
                resp.model = "mutated-by-hook".to_string();
            }
            PhaseOutcome::Continue
        }
    }

    #[tokio::test]
    async fn all_quota_exhausted_targets_return_service_unavailable() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-quota-exhausted-dispatch-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let provider = gw
            .admin()
            .create_provider(CreateProvider {
                name: format!("quota-provider-{}", uuid::Uuid::new_v4()),
                vendor: Some("openai".to_string()),
                protocol: "openai-compatible".to_string(),
                base_url: "http://127.0.0.1:9/v1".to_string(),
                protocol_mode: "fixed".to_string(),
                protocol_endpoints: Vec::new(),
                preset_key: None,
                channel: None,
                models_source: None,
                static_models: None,
                api_key: "sk-test".to_string(),
                auth_mode: "apikey".to_string(),
                use_proxy: false,
                fast_mode: false,
            })
            .await
            .expect("provider create");
        let route_name = format!("quota-route-{}", uuid::Uuid::new_v4());
        gw.admin()
            .create_model(CreateModel {
                name: route_name.clone(),
                balance: Some("priority".to_string()),
                target_provider: provider.id.clone(),
                target_model: "upstream-model".to_string(),
                targets: vec![CreateModelBackend {
                    provider_id: provider.id.clone(),
                    model: "upstream-model".to_string(),
                    weight: Some(100),
                    priority: Some(1),
                }],
                enable_auth: Some(false),
                enable_payload: None,
            })
            .await
            .expect("model create");
        gw.quota_registry.observe(
            &provider.id,
            &[QuotaTierObservation {
                name: "five_hour".to_string(),
                used_percent: 100.0,
                resets_at: None,
            }],
            None,
        );

        let envelope = RawEnvelope::new(
            Some(serde_json::json!({"model": route_name})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        );
        let request = AiRequest::new(route_name, Vec::new());
        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn local_server_errors_are_neutral_for_provider_health() {
        let local = super::error_response(500, "local parse error");
        let upstream = axum::response::IntoResponse::into_response((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream error",
        ));

        assert_eq!(
            super::health_outcome_from_response(&local),
            super::HealthOutcome::Neutral
        );
        assert_eq!(
            super::health_outcome_from_response(&upstream),
            super::HealthOutcome::Failure
        );
    }

    #[test]
    fn on_response_hook_errors_are_neutral_for_provider_health() {
        let short_circuit = super::normalize_phase_outcome(
            Phase::OnResponse,
            PhaseOutcome::ShortCircuit(
                (StatusCode::BAD_GATEWAY, "hook short circuit").into_response(),
            ),
        );
        let PhaseOutcome::ShortCircuit(short_circuit) = short_circuit else {
            panic!("short circuit should remain a response");
        };
        assert_eq!(
            super::health_outcome_from_response(&short_circuit),
            super::HealthOutcome::Neutral
        );

        let reject = super::normalize_phase_outcome(
            Phase::OnResponse,
            PhaseOutcome::Reject(crate::error::GatewayError::ProviderUnavailable {
                provider: "hook".to_string(),
                reason: "local rejection".to_string(),
            }),
        );
        let PhaseOutcome::ShortCircuit(reject) = reject else {
            panic!("reject should render as a response");
        };
        assert_eq!(
            super::health_outcome_from_response(&reject),
            super::HealthOutcome::Neutral
        );

        let upstream_phase = super::normalize_phase_outcome(
            Phase::OnUpstream,
            PhaseOutcome::ShortCircuit(
                (StatusCode::BAD_GATEWAY, "upstream phase response").into_response(),
            ),
        );
        let PhaseOutcome::ShortCircuit(upstream_phase) = upstream_phase else {
            panic!("short circuit should remain a response");
        };
        assert_eq!(
            super::health_outcome_from_response(&upstream_phase),
            super::HealthOutcome::Failure
        );
    }

    #[tokio::test]
    async fn complete_native_stream_resets_provider_health() {
        let registry = Arc::new(crate::router::health::HealthRegistry::new());
        let health_key = "native-stream-success";
        registry.try_acquire(health_key).unwrap().failure();
        registry.try_acquire(health_key).unwrap().failure();
        let permit = registry.try_acquire(health_key).unwrap();
        let ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-health\",\"model\":\"test\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(sse))
            .unwrap();

        let response = super::defer_stream_health(
            response,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &ctx,
            permit,
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), sse.as_bytes());

        registry.try_acquire(health_key).unwrap().failure();
        registry.try_acquire(health_key).unwrap().failure();
        assert!(registry.try_acquire(health_key).is_some());
    }

    #[tokio::test]
    async fn truncated_native_stream_fails_provider_health() {
        let registry = Arc::new(crate::router::health::HealthRegistry::new());
        let health_key = "native-stream-truncated";
        registry.try_acquire(health_key).unwrap().failure();
        registry.try_acquire(health_key).unwrap().failure();
        let permit = registry.try_acquire(health_key).unwrap();
        let ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let partial = "data: {\"id\":\"chatcmpl-health\",\"model\":\"test\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(partial))
            .unwrap();

        let response = super::defer_stream_health(
            response,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &ctx,
            permit,
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), partial.as_bytes());
        assert!(!registry.is_healthy(health_key));
        assert!(registry.try_acquire(health_key).is_none());
    }

    #[tokio::test]
    async fn hanging_native_stream_deadline_fails_provider_health() {
        let registry = Arc::new(crate::router::health::HealthRegistry::new());
        let health_key = "native-stream-deadline";
        registry.try_acquire(health_key).unwrap().failure();
        registry.try_acquire(health_key).unwrap().failure();
        let permit = registry.try_acquire(health_key).unwrap();
        let ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_millis(20),
        );
        let pending = futures::stream::pending::<Result<bytes::Bytes, std::convert::Infallible>>();
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from_stream(pending))
            .unwrap();

        let response = super::defer_stream_health(
            response,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &ctx,
            permit,
        );
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("response body should close at the request deadline")
        .unwrap();
        assert!(body.is_empty());
        assert!(!registry.is_healthy(health_key));
        assert!(registry.try_acquire(health_key).is_none());
    }

    #[tokio::test]
    async fn dropped_native_stream_response_is_neutral_for_provider_health() {
        let registry = Arc::new(crate::router::health::HealthRegistry::new());
        let health_key = "native-stream-client-disconnect";
        registry.try_acquire(health_key).unwrap().failure();
        registry.try_acquire(health_key).unwrap().failure();
        let permit = registry.try_acquire(health_key).unwrap();
        let ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let (source_tx, source_rx) = tokio::sync::mpsc::channel(1);
        source_tx
            .send(Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                b"data: {\"id\":\"chatcmpl-health\",\"model\":\"test\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            )))
            .await
            .unwrap();
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from_stream(
                tokio_stream::wrappers::ReceiverStream::new(source_rx),
            ))
            .unwrap();

        let response = super::defer_stream_health(
            response,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            &ctx,
            permit,
        );
        drop(response);
        tokio::time::timeout(std::time::Duration::from_secs(1), source_tx.closed())
            .await
            .expect("dropping the client body should stop the observer");
        assert!(registry.try_acquire(health_key).is_some());
    }

    #[tokio::test]
    async fn on_response_full_hook_can_mutate_response_body() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-onresponse-hook-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        let host = HostContext::new(&gw);
        let mut req_ctx = crate::proxy::context::RequestContext::new(
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            std::time::Duration::from_secs(30),
        );
        let mut request = AiRequest::new("orig-model", Vec::new());
        let mut resp = AiResponse::new("resp-1", "orig-model");

        let hook: Arc<dyn PhaseHook> = Arc::new(FullMutateHook);
        let hooks = [&hook];
        let outcome = run_phase_hooks_slice(
            &hooks,
            &mut req_ctx,
            &mut request,
            ResponseView::Full(&mut resp),
            &host,
        )
        .await;

        assert!(matches!(outcome, PhaseOutcome::Continue));
        assert_eq!(
            resp.model, "mutated-by-hook",
            "OnResponse Full hook must mutate the response in place"
        );
    }

    #[tokio::test]
    async fn dispatch_logs_reasoning_effort_snapshot_from_ir() {
        use crate::protocol::ir::{ReasoningConfig, ReasoningEffort};

        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-reasoning-effort-log-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, mut log_rx) = Gateway::new(config).await.expect("gateway init");
        let envelope = RawEnvelope::new(
            Some(serde_json::json!({"model": "missing-model"})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        );
        let mut request = AiRequest::new("missing-model", Vec::new());
        request.reasoning = ReasoningConfig {
            enabled: true,
            effort: Some(ReasoningEffort::High),
            ..Default::default()
        };

        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("log entry should be emitted")
            .expect("log channel should remain open");
        assert_eq!(
            entry.reasoning_effort.as_deref(),
            Some("high"),
            "normalized IR effort must be logged even on the error path"
        );
    }

    #[tokio::test]
    async fn dispatch_logs_reasoning_effort_none_when_not_declared() {
        let config = crate::config::GatewayConfig {
            data_dir: std::env::temp_dir().join(format!(
                "nyro-reasoning-effort-none-test-{}",
                uuid::Uuid::new_v4()
            )),
            ..Default::default()
        };
        let (gw, mut log_rx) = Gateway::new(config).await.expect("gateway init");
        let envelope = RawEnvelope::new(
            Some(serde_json::json!({"model": "missing-model"})),
            HashMap::new(),
            "POST",
            "/v1/chat/completions",
        );
        let request = AiRequest::new("missing-model", Vec::new());

        let response = dispatch_pipeline(
            gw,
            HeaderMap::new(),
            envelope,
            request,
            OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            crate::proxy::context::RequestContext::new(
                OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
                std::time::Duration::from_secs(30),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let entry = tokio::time::timeout(std::time::Duration::from_secs(1), log_rx.recv())
            .await
            .expect("log entry should be emitted")
            .expect("log channel should remain open");
        assert_eq!(entry.reasoning_effort, None);
    }
}
