use axum::Json;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::proxy::security::is_key_expired;
use crate::storage::traits::DynStorage;

/// Auth material injected by the proxy router: the startup-configured gateway
/// key plus a handle to the api-key store, so the middleware can also accept
/// DB-managed keys created in the admin WebUI.
#[derive(Clone)]
pub struct ProxyAuth {
    pub gateway_key: String,
    pub storage: DynStorage,
}

pub async fn bearer_auth(request: Request, next: Next) -> Response {
    let Some(auth) = request.extensions().get::<ProxyAuth>().cloned() else {
        return next.run(request).await;
    };

    let expected = auth.gateway_key.trim();
    if expected.is_empty() {
        return next.run(request).await;
    }

    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token = header.strip_prefix("Bearer ").unwrap_or("").trim();

    if token == expected {
        return next.run(request).await;
    }

    // Also accept DB-managed API keys (admin WebUI "API Key" page) so the
    // per-model binding/quota checks in the dispatcher stay reachable.
    // The per-model layer re-validates and returns precise errors.
    if !token.is_empty()
        && let Some(store) = auth.storage.auth()
        && let Ok(Some(key)) = store.find_api_key(token).await
        && key.is_enabled
        && key
            .expires_at
            .as_ref()
            .map(|expires| !is_key_expired(expires))
            .unwrap_or(true)
    {
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {
                "message": "Invalid API key",
                "type": "NYRO_AUTH_ERROR",
                "code": "invalid_api_key"
            }
        })),
    )
        .into_response()
}
