//! Refresh-only transport classification. Never retain a source error, URL, or response body.

use std::fmt;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, StatusCode};
use serde::Deserialize;

/// Whether a rotating refresh token can safely be used again is not implied by
/// an HTTP or transport error. In particular, unknown outcomes must not be
/// treated as a rejected grant (the server may already have rotated the token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshFailure {
    PreSend,
    OutcomeUnknown { stage: &'static str },
    Rejected { status: u16 },
    RateLimited { retry_at: DateTime<Utc> },
}

impl fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreSend => f.write_str("OAuth token refresh failed before sending the request"),
            // Stage is an internal diagnostic, not an upstream-controlled message.
            Self::OutcomeUnknown { .. } => f.write_str("OAuth token refresh outcome is unknown"),
            Self::Rejected { status } => {
                write!(f, "OAuth token refresh was rejected (HTTP {status})")
            }
            Self::RateLimited { .. } => f.write_str("OAuth token refresh was rate limited"),
        }
    }
}

impl std::error::Error for RefreshFailure {}

/// Execute exactly one refresh attempt and discard all sensitive error details.
///
/// The caller MUST supply a client with redirects disabled and
/// automatic retries disabled (`retry(reqwest::retry::never())`). Only under
/// that contract does `is_connect()` prove that the refresh POST was not sent.
/// The existing provider normalizer remains responsible for tokens and identity.
pub(crate) async fn send_refresh_request<T>(
    request: RequestBuilder,
    normalize: impl FnOnce(&str) -> anyhow::Result<T>,
) -> Result<T, RefreshFailure> {
    let (client, request) = request.build_split();
    let request = request.map_err(|_| RefreshFailure::PreSend)?;
    let response = client.execute(request).await.map_err(|error| {
        if error.is_connect() {
            RefreshFailure::PreSend
        } else {
            RefreshFailure::OutcomeUnknown { stage: "send" }
        }
    })?;
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        // The status and headers suffice; even a damaged body cannot turn a
        // definite rate limit into a generic error or leak upstream content.
        return Err(RefreshFailure::RateLimited {
            retry_at: retry_at(response.headers(), Utc::now()),
        });
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| RefreshFailure::OutcomeUnknown { stage: "read" })?;
    let body = std::str::from_utf8(&body)
        .map_err(|_| RefreshFailure::OutcomeUnknown { stage: "decode" })?;
    if !status.is_success() {
        return Err(classify_response(status, body));
    }
    normalize(body).map_err(|_| RefreshFailure::OutcomeUnknown { stage: "decode" })
}

fn classify_response(status: StatusCode, body: &str) -> RefreshFailure {
    #[derive(Deserialize)]
    struct OAuthError {
        error: String,
    }

    // A status alone (including a proxy's 400/401/403), free-form message,
    // nested vendor error, or unknown code is not proof of a rejected grant.
    let rejected = matches!(status.as_u16(), 400 | 401 | 403)
        && serde_json::from_str::<OAuthError>(body).is_ok_and(|error| {
            matches!(
                error.error.as_str(),
                "invalid_request"
                    | "invalid_client"
                    | "invalid_grant"
                    | "unauthorized_client"
                    | "unsupported_grant_type"
                    | "invalid_scope"
                    | "access_denied"
            )
        });
    if rejected {
        RefreshFailure::Rejected {
            status: status.as_u16(),
        }
    } else {
        RefreshFailure::OutcomeUnknown { stage: "response" }
    }
}

fn retry_at(headers: &HeaderMap, now: DateTime<Utc>) -> DateTime<Utc> {
    let delay = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let value = value.trim();
            bounded_integer(value, 3600)
                .map(Duration::seconds)
                .or_else(|| http_date(value).map(|date| date - now))
        })
        .or_else(|| {
            headers
                .get("retry-after-ms")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| bounded_integer(value.trim(), 3_600_000))
                .map(Duration::milliseconds)
        })
        .unwrap_or_else(|| Duration::seconds(60));
    now + delay.clamp(Duration::seconds(1), Duration::hours(1))
}

fn bounded_integer(value: &str, maximum: u64) -> Option<i64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A valid but overflowing integer still means "at least one hour", not
    // the shorter default cooldown.
    Some(value.parse::<u64>().unwrap_or(u64::MAX).min(maximum) as i64)
}

pub(crate) fn http_date(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&Utc))
        .or_else(|| {
            // HTTP recipients also accept the obsolete RFC 850/asctime forms.
            ["%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"]
                .into_iter()
                .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
                .map(|date| date.and_utc())
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    pub(crate) fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap()
    }

    /// One bounded loopback request, including its complete request body.
    /// An empty reply simulates a connection lost after receiving the request.
    pub(crate) async fn raw_response(response: Vec<u8>) -> (RequestBuilder, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "request ended before its body");
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or_default();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                stream.write_all(&response).await.unwrap();
                stream.shutdown().await.unwrap();
                request
            })
            .await
            .expect("loopback request timed out")
        });
        (
            client().post(format!("http://{address}/token?secret-url-marker")),
            server,
        )
    }

    pub(crate) async fn response(
        status: u16,
        headers: &str,
        body: &[u8],
    ) -> (RequestBuilder, JoinHandle<Vec<u8>>) {
        let mut response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        raw_response(response).await
    }

    fn now() -> DateTime<Utc> {
        "1994-11-06T08:49:00Z".parse().unwrap()
    }

    fn delay(headers: &[(&'static str, &str)]) -> Duration {
        let headers = headers
            .iter()
            .map(|(key, value)| {
                (
                    reqwest::header::HeaderName::from_static(key),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect();
        retry_at(&headers, now()) - now()
    }

    #[test]
    fn retry_after_seconds_bounds_and_fallback() {
        for (value, seconds) in [
            ("42", 42),
            (" 42 ", 42),
            ("0", 1),
            ("3601", 3600),
            ("999999999999999999999999999999999999999", 3600),
            ("", 60),
            ("-1", 60),
            ("NaN", 60),
            ("1.5", 60),
            ("secret-marker", 60),
        ] {
            assert_eq!(delay(&[("retry-after", value)]), Duration::seconds(seconds));
        }
        assert_eq!(delay(&[]), Duration::seconds(60));
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(retry_at(&headers, now()) - now(), Duration::seconds(60));
    }

    #[test]
    fn retry_after_http_dates_and_millisecond_fallback() {
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(delay(&[("retry-after", value)]), Duration::seconds(37));
        }
        assert_eq!(
            delay(&[("retry-after", "Sun, 06 Nov 1994 08:48:00 GMT")]),
            Duration::seconds(1)
        );
        assert_eq!(
            delay(&[("retry-after", "Mon, 07 Nov 1994 08:49:00 GMT")]),
            Duration::hours(1)
        );
        for (value, millis) in [
            ("0", 1000),
            ("1", 1000),
            ("1500", 1500),
            ("3600001", 3_600_000),
            ("invalid", 60_000),
        ] {
            assert_eq!(
                delay(&[("retry-after-ms", value)]),
                Duration::milliseconds(millis)
            );
        }
        assert_eq!(
            delay(&[("retry-after", "2"), ("retry-after-ms", "5000")]),
            Duration::seconds(2)
        );
        assert_eq!(
            delay(&[("retry-after", "invalid"), ("retry-after-ms", "1500")]),
            Duration::milliseconds(1500)
        );
    }

    #[test]
    fn only_standard_oauth_rejections_are_definite() {
        for status in [400, 401, 403] {
            for code in [
                "invalid_grant",
                "invalid_client",
                "invalid_request",
                "unauthorized_client",
                "unsupported_grant_type",
                "invalid_scope",
                "access_denied",
            ] {
                let body = serde_json::json!({"error": code, "error_description": "secret-marker https://secret.invalid"}).to_string();
                let error = classify_response(StatusCode::from_u16(status).unwrap(), &body);
                assert_eq!(error, RefreshFailure::Rejected { status });
                assert_sanitized(&error);
            }
        }
        for status in [400, 401, 403, 404, 500, 502, 503, 307] {
            for body in [
                r#"{"error":"server_error"}"#,
                r#"{"error":"temporarily_unavailable"}"#,
                r#"{"error":"unknown"}"#,
                r#"{"code":"invalid_grant"}"#,
                r#"{"error":{"code":"invalid_grant"}}"#,
                r#"{"error":"invalid_grant","error":"server_error"}"#,
                "<html>secret-marker</html>",
                "{",
                "{}",
            ] {
                assert!(matches!(
                    classify_response(StatusCode::from_u16(status).unwrap(), body),
                    RefreshFailure::OutcomeUnknown { .. }
                ));
            }
        }
        assert!(matches!(
            classify_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"invalid_grant"}"#
            ),
            RefreshFailure::OutcomeUnknown { .. }
        ));
    }

    fn normalize(body: &str) -> anyhow::Result<String> {
        let json: serde_json::Value = serde_json::from_str(body)?;
        json["access_token"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("missing token: {body}"))
    }

    pub(crate) fn assert_sanitized(error: &RefreshFailure) {
        let error = anyhow::Error::new(error.clone());
        assert!(error.downcast_ref::<RefreshFailure>().is_some());
        assert_eq!(error.chain().count(), 1);
        for text in [
            error.to_string(),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            assert!(text.is_ascii());
            assert!(!text.contains("secret"));
            assert!(!text.contains("http://"));
            assert!(!text.contains("https://"));
        }
    }

    #[tokio::test]
    async fn build_and_initial_connect_failures_are_pre_send() {
        let error = send_refresh_request(
            client()
                .post("http://127.0.0.1/token?secret-url-marker")
                .header("x-invalid", "secret-marker\n"),
            normalize,
        )
        .await
        .unwrap_err();
        assert_eq!(error, RefreshFailure::PreSend);
        assert_sanitized(&error);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = send_refresh_request(
            client().post(format!("http://{address}/secret-marker")),
            normalize,
        )
        .await
        .unwrap_err();
        assert_eq!(error, RefreshFailure::PreSend);
        assert_sanitized(&error);
    }

    #[tokio::test]
    async fn damaged_success_responses_never_retain_sensitive_parse_errors() {
        for body in [
            b"{secret-marker".as_slice(),
            b"{}",
            b"{\"access_token\":\"\"}",
            b"{\"access_token\":\"secret-marker\",\"broken\":}",
            b"\xffsecret-marker",
        ] {
            let (request, server) = response(200, "", body).await;
            let error = send_refresh_request(request.body("secret-refresh-marker"), normalize)
                .await
                .unwrap_err();
            assert_eq!(error, RefreshFailure::OutcomeUnknown { stage: "decode" });
            assert_sanitized(&error);
            let request = server.await.unwrap();
            assert!(String::from_utf8_lossy(&request).contains("secret-refresh-marker"));
        }
    }

    #[tokio::test]
    async fn lost_response_and_partial_body_are_unknown_without_replay() {
        for (response, stage) in [
            (Vec::new(), "send"),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\nsecret-marker".to_vec(),
                "read",
            ),
        ] {
            let (request, server) = raw_response(response).await;
            let error = send_refresh_request(request.body("secret-refresh-marker"), normalize)
                .await
                .unwrap_err();
            assert_eq!(error, RefreshFailure::OutcomeUnknown { stage });
            assert_sanitized(&error);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rate_limit_uses_headers_without_reading_a_damaged_body() {
        let (request, server) = raw_response(b"HTTP/1.1 429 Limited\r\nRetry-After-Ms: 1500\r\nContent-Length: 999\r\n\r\nsecret-marker".to_vec()).await;
        let before = Utc::now();
        let error = send_refresh_request(request, normalize).await.unwrap_err();
        let RefreshFailure::RateLimited { retry_at } = error else {
            panic!("expected rate limit")
        };
        assert!(retry_at >= before + Duration::milliseconds(1500));
        assert!(retry_at <= Utc::now() + Duration::milliseconds(1500));
        assert_sanitized(&error);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_server_errors_and_unknown_codes_are_not_replayed() {
        for (status, headers, body) in [
            (307, "Location: http://127.0.0.1:1/secret-marker\r\n", ""),
            (
                503,
                "",
                r#"{"error":"invalid_grant","error_description":"secret-marker"}"#,
            ),
            (
                400,
                "",
                r#"{"error":"server_error","message":"secret-marker"}"#,
            ),
        ] {
            let (request, server) = response(status, headers, body.as_bytes()).await;
            let error = send_refresh_request(request, normalize).await.unwrap_err();
            assert_eq!(error, RefreshFailure::OutcomeUnknown { stage: "response" });
            assert_sanitized(&error);
            server.await.unwrap();
        }
        let (request, server) = response(
            401,
            "",
            br#"{"error":"invalid_grant","error_description":"secret-marker"}"#,
        )
        .await;
        let error = send_refresh_request(request, normalize).await.unwrap_err();
        assert_eq!(error, RefreshFailure::Rejected { status: 401 });
        assert_sanitized(&error);
        server.await.unwrap();
    }
}
