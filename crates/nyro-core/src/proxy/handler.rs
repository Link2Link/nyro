//! Ingress handlers that remain in the legacy handler module.
//!
//! PR-16: The full proxy pipeline (`proxy_pipeline`, `handle_non_stream`,
//! `handle_stream`, etc.) has been moved to `proxy/dispatcher.rs`.
//! Old ingress handlers (`openai_proxy`, `anthropic_proxy`, etc.) have been
//! replaced by `proxy/ingress/*.rs` thin shells wired directly in `server.rs`.
//!
//! This file now contains only the read-only endpoints that do not go through
//! the proxy pipeline: `models_list` and `nyro_usage` (usage forwarding).

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::Gateway;
use crate::admin::RouteAccess;
use crate::proxy::security::{extract_api_key, is_key_expired};

/// Who is calling a read-only proxy endpoint.
struct ProxyCaller {
    /// The gateway key or an enabled, unexpired API key was presented.
    authenticated: bool,
    access: RouteAccess,
}

/// Classify the caller's credential with the same rules for every read-only
/// endpoint: the gateway key (`NYRO_PROXY_AUTH_KEY`) is a master key, a
/// privileged API key sees every route, a regular key its bound routes, and
/// anyone the open (auth-disabled) routes.
async fn proxy_caller(gw: &Gateway, headers: &HeaderMap) -> ProxyCaller {
    let token = extract_api_key(headers);
    let master_key = gw
        .config
        .auth_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    if token
        .as_deref()
        .is_some_and(|token| master_key == Some(token))
    {
        return ProxyCaller {
            authenticated: true,
            access: RouteAccess::privileged(),
        };
    }

    let mut caller = ProxyCaller {
        authenticated: false,
        access: RouteAccess::default(),
    };
    if let Some(raw_key) = token
        && let Some(store) = gw.storage.auth()
        && let Ok(Some(key_row)) = store.find_api_key(&raw_key).await
    {
        let key_active = key_row.is_enabled
            && key_row
                .expires_at
                .as_ref()
                .map(|expires| !is_key_expired(expires))
                .unwrap_or(true);
        if key_active {
            caller.authenticated = true;
            if key_row.is_privileged {
                caller.access.privileged = true;
            } else if let Ok(bound_route_ids) = store.list_bound_model_ids(&key_row.id).await {
                caller.access.bound_route_ids.extend(bound_route_ids);
            }
        }
    }
    caller
}

fn json_error(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"message": message, "type": "NYRO_AUTH_ERROR", "code": code}
        })),
    )
        .into_response()
}

// ── GET /v1/models ────────────────────────────────────────────────────────────

pub async fn models_list(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let access = proxy_caller(&gw, &headers).await.access;
    let cache = gw.model_cache.read().await;
    let models = cache
        .models
        .iter()
        .filter(|model| access.can_route(model))
        .map(|model| model.name.trim())
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();

    let data = models
        .into_iter()
        .map(|model| {
            serde_json::json!({
                "id": model,
                "object": "model",
                "created": 0,
                "owned_by": "Nyro"
            })
        })
        .collect::<Vec<_>>();

    Json(serde_json::json!({
        "object": "list",
        "data": data
    }))
    .into_response()
}

// ── GET /v1/nyro/usage ────────────────────────────────────────────────────────

/// Usage forwarding for downstream Nyro instances (the Nyro preset). Requires
/// a recognised key even when the proxy is open: upstream account usage is
/// never served anonymously. Scope and redaction follow the key's route access.
pub async fn nyro_usage(State(gw): State<Gateway>, headers: HeaderMap) -> Response {
    let caller = proxy_caller(&gw, &headers).await;
    if !caller.authenticated {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "usage forwarding requires the gateway key or an enabled API key",
            "invalid_api_key",
        );
    }
    match gw.admin().forwarded_provider_usage(&caller.access).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => {
            tracing::warn!(%error, "usage forwarding report failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {"message": error.to_string(), "type": "NYRO_INTERNAL_ERROR"}
                })),
            )
                .into_response()
        }
    }
}
