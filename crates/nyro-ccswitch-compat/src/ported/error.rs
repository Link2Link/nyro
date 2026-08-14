// Minimal protocol-conversion subset of cc-switch's ProxyError.
// Copyright (c) 2025 Jason Young. Licensed under MIT.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("服务器已在运行")]
    AlreadyRunning,
    #[error("服务器未运行")]
    NotRunning,
    #[error("请求转发失败: {0}")]
    ForwardFailed(String),
    #[error("无可用的Provider")]
    NoAvailableProvider,
    #[error("所有供应商已熔断，无可用渠道")]
    AllProvidersCircuitOpen,
    #[error("未配置供应商")]
    NoProvidersConfigured,
    #[error("Provider不健康: {0}")]
    ProviderUnhealthy(String),
    #[error("上游错误 (状态码 {status}): {body:?}")]
    UpstreamError { status: u16, body: Option<String> },
    #[error("超过最大重试次数")]
    MaxRetriesExceeded,
    #[error("数据库错误: {0}")]
    DatabaseError(String),
    #[error("配置错误: {0}")]
    ConfigError(String),
    #[error("格式转换错误: {0}")]
    TransformError(String),
    #[error("无效的请求: {0}")]
    InvalidRequest(String),
    #[error("超时: {0}")]
    Timeout(String),
    #[error("流式响应空闲超时: {0}秒无数据")]
    StreamIdleTimeout(u64),
    #[error("认证失败: {0}")]
    AuthError(String),
    #[error("内部错误: {0}")]
    Internal(String),
}

pub(crate) fn map_proxy_error_to_status(error: &ProxyError) -> u16 {
    match error {
        ProxyError::AlreadyRunning => 409,
        ProxyError::NotRunning => 503,
        ProxyError::UpstreamError { status, .. } => *status,
        ProxyError::Timeout(_) | ProxyError::StreamIdleTimeout(_) => 504,
        ProxyError::ForwardFailed(_) => 502,
        ProxyError::NoAvailableProvider
        | ProxyError::AllProvidersCircuitOpen
        | ProxyError::NoProvidersConfigured
        | ProxyError::MaxRetriesExceeded
        | ProxyError::ProviderUnhealthy(_) => 503,
        ProxyError::ConfigError(_) | ProxyError::InvalidRequest(_) => 400,
        ProxyError::AuthError(_) => 401,
        ProxyError::TransformError(_) => 422,
        ProxyError::DatabaseError(_) | ProxyError::Internal(_) => 500,
    }
}

pub(crate) fn get_error_message(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamError { status, body } => body
            .as_ref()
            .map(|body| format!("上游错误 ({status}): {body}"))
            .unwrap_or_else(|| format!("上游错误 ({status})")),
        ProxyError::Timeout(message) => format!("请求超时: {message}"),
        ProxyError::ForwardFailed(message) => format!("转发失败: {message}"),
        ProxyError::NoAvailableProvider => "无可用 Provider".to_string(),
        ProxyError::AllProvidersCircuitOpen => "所有供应商已熔断，无可用渠道".to_string(),
        ProxyError::NoProvidersConfigured => "未配置供应商".to_string(),
        ProxyError::MaxRetriesExceeded => "所有 Provider 都失败，重试耗尽".to_string(),
        ProxyError::ProviderUnhealthy(message) => format!("Provider 不健康: {message}"),
        ProxyError::DatabaseError(message) => format!("数据库错误: {message}"),
        ProxyError::TransformError(message) => format!("请求/响应转换错误: {message}"),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_upstream_error() {
        let error = ProxyError::UpstreamError {
            status: 401,
            body: Some("Unauthorized".to_string()),
        };
        assert_eq!(map_proxy_error_to_status(&error), 401);
    }

    #[test]
    fn test_map_timeout_error() {
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::Timeout("Request timeout".to_string())),
            504
        );
    }

    #[test]
    fn test_map_connection_error() {
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::ForwardFailed("Connection refused".to_string())),
            502
        );
    }

    #[test]
    fn test_map_no_provider_error() {
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::NoAvailableProvider),
            503
        );
    }

    #[test]
    fn test_map_status_matches_proxy_error_response_semantics() {
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::AuthError("bad token".to_string())),
            401
        );
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::ConfigError("bad config".to_string())),
            400
        );
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::InvalidRequest("bad request".to_string())),
            400
        );
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::TransformError("bad transform".to_string())),
            422
        );
        assert_eq!(
            map_proxy_error_to_status(&ProxyError::StreamIdleTimeout(30)),
            504
        );
    }

    #[test]
    fn test_get_error_message() {
        let error = ProxyError::UpstreamError {
            status: 500,
            body: Some("Internal Server Error".to_string()),
        };
        let message = get_error_message(&error);
        assert!(message.contains("上游错误"));
        assert!(message.contains("500"));
        assert!(message.contains("Internal Server Error"));
    }
}
