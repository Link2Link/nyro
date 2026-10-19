//! Thin wrapper around `reqwest::Client` for upstream calls.
//!
//! PR3 split out the old `ProviderAdapter` plumbing — URL building and
//! auth header construction now happen at the call site (via
//! `VendorRegistry::resolve` + `VendorExtension::{auth_headers,
//! build_url}`). `ProxyClient` is intentionally adapter-agnostic: it
//! takes a fully-built URL and a ready-to-send header map and just
//! issues the HTTP call.

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;

mod timeouts;
use crate::config::UpstreamTimeouts;
use crate::db::models::Provider;
use crate::proxy::context::Deadline;
use timeouts::{TimedReadStream, bounded_timeout};
pub use timeouts::{UpstreamReadError, UpstreamTimeoutPhase};

pub const MAX_UPSTREAM_RESPONSE_BODY_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct ProxyClient {
    pub http: reqwest::Client,
    performance: Option<crate::performance::Attempt>,
    timeouts: UpstreamTimeouts,
    deadline: Deadline,
    /// Extra attempts after a connect failure that happened before the request
    /// was written. Zero preserves the historical single-attempt behavior.
    connect_retries: u8,
}

pub struct ObservedResponse {
    response: reqwest::Response,
    performance: Option<crate::performance::Attempt>,
    timeouts: UpstreamTimeouts,
    deadline: Deadline,
    headers_at: tokio::time::Instant,
}
impl ObservedResponse {
    pub fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }
    pub fn bytes_stream(
        self,
    ) -> impl futures::Stream<Item = Result<Bytes, UpstreamReadError>> + Send + Unpin {
        let stream = TimedReadStream::new(
            self.response.bytes_stream(),
            self.timeouts,
            self.deadline,
            self.headers_at,
        );
        match self.performance {
            Some(attempt) => futures::future::Either::Left(attempt.observe(stream)),
            None => futures::future::Either::Right(stream),
        }
    }
    pub async fn bytes(self) -> Result<Bytes> {
        let mut stream = Box::pin(self.bytes_stream());
        let mut bytes = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok(bytes.freeze())
    }
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T> {
        Ok(serde_json::from_slice(&self.bytes().await?)?)
    }
}

#[derive(Debug)]
pub struct RawUpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Debug, thiserror::Error)]
#[error("error decoding response body: {source}")]
pub struct UpstreamResponseDecodeError {
    pub source: serde_json::Error,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

impl UpstreamResponseDecodeError {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl ProxyClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            performance: None,
            timeouts: UpstreamTimeouts::default(),
            deadline: Deadline::never(),
            connect_retries: 0,
        }
    }

    pub fn with_timeouts(mut self, timeouts: UpstreamTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn with_connect_retries(mut self, retries: u8) -> Self {
        self.connect_retries = retries;
        self
    }

    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = deadline;
        self
    }

    pub(crate) fn with_performance(mut self, attempt: crate::performance::Attempt) -> Self {
        self.performance = Some(attempt);
        self
    }

    pub async fn call_non_stream_raw(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<RawUpstreamResponse> {
        ensure_json_content_type(&mut headers);
        let response = self.call_stream_raw(url, headers, body).await?;
        Self::buffer_response(response).await
    }

    pub async fn buffer_response(response: ObservedResponse) -> Result<RawUpstreamResponse> {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = read_body_with_limit(response, MAX_UPSTREAM_RESPONSE_BODY_BYTES).await?;
        Ok(RawUpstreamResponse {
            status,
            headers,
            body,
        })
    }

    pub async fn call_non_stream(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Value,
    ) -> Result<(Value, u16, HeaderMap)> {
        ensure_json_content_type(&mut headers);
        let response = self
            .call_stream_raw(url, headers, serde_json::to_vec(&body)?.into())
            .await?;
        let status = response.status().as_u16();
        let mut headers = response.headers().clone();
        // Some gateways force-compress even when the client never advertised
        // accept-encoding; decode a supported content-encoding before JSON
        // parsing instead of failing with a misleading decode error.
        let encoding = headers
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase);
        let body = match encoding
            .as_deref()
            .filter(|encoding| nyro_ccswitch_compat::is_supported_content_encoding(encoding))
        {
            Some(encoding) => {
                let compressed = response.bytes().await?;
                match nyro_ccswitch_compat::decompress_body_with_limit(
                    encoding,
                    &compressed,
                    MAX_UPSTREAM_RESPONSE_BODY_BYTES,
                ) {
                    Ok(Some(decompressed)) => {
                        headers.remove(reqwest::header::CONTENT_ENCODING);
                        headers.remove(reqwest::header::CONTENT_LENGTH);
                        Bytes::from(decompressed)
                    }
                    Err(error) => {
                        if let Some(attempt) = &self.performance {
                            attempt.record_failure(
                                "failed",
                                "decompression_error",
                                "upstream_decode",
                                &error,
                            );
                        }
                        return Err(error.into());
                    }
                    Ok(None) => compressed,
                }
            }
            None => response.bytes().await?,
        };
        let json: Value = match serde_json::from_slice(&body) {
            Ok(json) => json,
            Err(source) => {
                if let Some(attempt) = &self.performance {
                    attempt.record_failure(
                        "failed",
                        "response_parse_error",
                        "upstream_decode",
                        &source,
                    );
                }
                return Err(UpstreamResponseDecodeError {
                    source,
                    status,
                    headers,
                    body,
                }
                .into());
            }
        };
        Ok((json, status, headers))
    }

    pub async fn call_stream_raw(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<ObservedResponse> {
        ensure_json_content_type(&mut headers);
        if let Some(attempt) = &self.performance {
            attempt.request(&body);
            attempt.request_headers(&headers);
        }
        let mut retries_left = self.connect_retries;
        let response = loop {
            let (until, phase) = bounded_timeout(
                tokio::time::Instant::now() + self.timeouts.response_headers,
                UpstreamTimeoutPhase::ResponseHeaders,
                &self.deadline,
            );
            let response = if until <= tokio::time::Instant::now() {
                Err(UpstreamReadError::Timeout { phase })
            } else {
                match tokio::time::timeout_at(
                    until,
                    self.http
                        .post(url)
                        .headers(headers.clone())
                        .body(body.clone())
                        .send(),
                )
                .await
                {
                    Ok(result) => result.map_err(UpstreamReadError::from),
                    Err(_) => Err(UpstreamReadError::Timeout { phase }),
                }
            };
            match response {
                Ok(response) => break response,
                Err(error)
                    if retries_left > 0
                        && error.is_transient_connect()
                        && self.deadline.remaining() >= std::time::Duration::from_secs(1) =>
                {
                    retries_left -= 1;
                    // The handshake died before HTTP, so the upstream did not
                    // see this body. One short pause lets a fresh connection
                    // through a proxy that just dropped the previous one.
                    tracing::info!(
                        "transient upstream connect failure before the request was sent; retrying"
                    );
                    tokio::time::sleep(TRANSIENT_CONNECT_RETRY_BACKOFF).await;
                }
                Err(error) => {
                    if let Some(attempt) = &self.performance {
                        attempt.record_failure(
                            if error.is_timeout() {
                                "timed_out"
                            } else {
                                "failed"
                            },
                            if error.is_timeout() {
                                "timeout"
                            } else {
                                "upstream_connect_error"
                            },
                            if error.is_timeout() {
                                error.stage()
                            } else {
                                "upstream_connect"
                            },
                            &error,
                        );
                    }
                    return Err(error.into());
                }
            }
        };
        if let Some(attempt) = &self.performance {
            attempt.response(response.status().as_u16(), response.headers());
        }
        Ok(ObservedResponse {
            response,
            performance: self.performance.clone(),
            timeouts: self.timeouts,
            deadline: self.deadline.clone(),
            headers_at: tokio::time::Instant::now(),
        })
    }

    pub async fn call_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(ObservedResponse, u16)> {
        let resp = self
            .call_stream_raw(url, headers, serde_json::to_vec(&body)?.into())
            .await?;
        let status = resp.status().as_u16();
        Ok((resp, status))
    }
}

async fn read_body_with_limit(response: ObservedResponse, limit: usize) -> Result<Bytes> {
    let performance = response.performance.clone();
    let mut stream = response.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > limit {
            if let Some(p) = &performance {
                p.record_message(
                    "failed",
                    "response_size_limit",
                    "upstream_read",
                    "Upstream body exceeds the processing size limit.",
                );
            }
            anyhow::bail!("upstream response body exceeds {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// Pause before replaying a connect failure. Long enough to leave a proxy
/// that just reset the handshake, short enough to stay inside one user turn.
const TRANSIENT_CONNECT_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// How many extra connect attempts a provider's channel may make.
///
/// These channels are reached through whatever DNS the host uses. On a
/// fake-ip proxy the first TLS handshake of a fresh connection is
/// occasionally reset (`tls handshake eof`) before the HTTP request is
/// written — observed in production against `cli-chat-proxy.grok.com`
/// (xai/grok) and `server.codeium.com` (devin/cli). The route usually has
/// a single warm candidate, and a connect failure proves the body was not
/// sent, so replaying such a channel once is safe. Other vendors and
/// channels stay single-attempt until they show the same signature.
pub(crate) fn transient_connect_retries(provider: &Provider) -> u8 {
    let is_channel = |vendor: &str, channel: &str| {
        provider
            .vendor
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case(vendor))
            && provider
                .channel
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(channel))
    };
    if is_channel("xai", "grok") || is_channel("devin", "cli") {
        1
    } else {
        0
    }
}

fn ensure_json_content_type(headers: &mut HeaderMap) {
    if !headers.contains_key(CONTENT_TYPE) {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    async fn serve_once(response: &[u8]) -> (String, oneshot::Receiver<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let response = response.to_vec();
        let (request_tx, request_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let request = read_request(&mut socket).await;
            let _ = request_tx.send(request);
            socket.write_all(&response).await.expect("write response");
            socket.shutdown().await.expect("shutdown response");
        });
        (
            format!("http://{addr}/v1beta/models/gemini:generateContent?key=secret"),
            request_rx,
        )
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut expected_len = None;

        loop {
            let mut buf = [0_u8; 1024];
            let read = socket.read(&mut buf).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);

            if expected_len.is_none()
                && let Some(header_end) = header_end(&request)
            {
                let content_length = request_header(&request, "content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or_default();
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|len| request.len() >= len) {
                break;
            }
        }

        request
    }

    fn header_end(message: &[u8]) -> Option<usize> {
        message.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn request_header<'a>(request: &'a [u8], name: &str) -> Option<&'a str> {
        let header_end = header_end(request)?;
        std::str::from_utf8(&request[..header_end])
            .ok()?
            .lines()
            .skip(1)
            .find_map(|line| {
                let (header_name, value) = line.split_once(':')?;
                header_name.eq_ignore_ascii_case(name).then(|| value.trim())
            })
    }

    fn request_body(request: &[u8]) -> &[u8] {
        let body_start = header_end(request).expect("request headers") + 4;
        &request[body_start..]
    }

    #[tokio::test]
    async fn non_stream_decodes_gzip_content_encoding_response() {
        // A gateway that force-compresses without being asked must not break
        // the buffered JSON path.
        let json = br#"{"id":"chatcmpl_gz","choices":[{"index":0,"message":{"role":"assistant","content":"GZIP-OK"},"finish_reason":"stop"}]}"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, json).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            compressed.len()
        )
        .into_bytes();
        response.extend_from_slice(&compressed);
        let (url, _captured) = serve_once(&response).await;
        let client = ProxyClient::new(reqwest::Client::new());

        let (value, status, headers) = client
            .call_non_stream(
                &url,
                HeaderMap::new(),
                serde_json::json!({"model": "m", "messages": []}),
            )
            .await
            .expect("gzip response decodes");
        assert_eq!(status, 200);
        assert_eq!(value["choices"][0]["message"]["content"], "GZIP-OK");
        assert!(headers.get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn raw_non_stream_preserves_request_and_response_bytes() {
        const REQUEST_BODY: &[u8] = b"{ \"z\": 1,\n  \"a\" : [true, null] }\n";
        const RESPONSE_BODY: &[u8] = b"{ \"second\": 2, \"first\": 1 }\n";
        let mut response = format!(
            "HTTP/1.1 202 Accepted\r\ncontent-type: application/octet-stream\r\nx-upstream-id: raw-123\r\ncontent-length: {}\r\n\r\n",
            RESPONSE_BODY.len()
        )
        .into_bytes();
        response.extend_from_slice(RESPONSE_BODY);
        let (url, captured_request) = serve_once(&response).await;
        let client = ProxyClient::new(reqwest::Client::new());

        let upstream = client
            .call_non_stream_raw(&url, HeaderMap::new(), Bytes::from_static(REQUEST_BODY))
            .await
            .expect("raw upstream call");
        let captured_request = captured_request.await.expect("captured request");

        assert_eq!(request_body(&captured_request), REQUEST_BODY);
        assert_eq!(
            request_header(&captured_request, "content-type"),
            Some("application/json")
        );
        assert_eq!(upstream.status, 202);
        assert_eq!(
            upstream
                .headers
                .get("x-upstream-id")
                .and_then(|value| value.to_str().ok()),
            Some("raw-123")
        );
        assert_eq!(upstream.body.as_ref(), RESPONSE_BODY);
    }

    #[tokio::test]
    async fn raw_stream_preserves_explicit_content_type() {
        const REQUEST_BODY: &[u8] = b"{\"second\":2, \"first\":1}";
        const RESPONSE_BODY: &[u8] = b"data: {\"ok\":true}\n\n";
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
            RESPONSE_BODY.len()
        )
        .into_bytes();
        response.extend_from_slice(RESPONSE_BODY);
        let (url, captured_request) = serve_once(&response).await;
        let client = ProxyClient::new(reqwest::Client::new());
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.test+json"),
        );

        let response = client
            .call_stream_raw(&url, headers, Bytes::from_static(REQUEST_BODY))
            .await
            .expect("raw streaming call");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            response.bytes().await.expect("response body"),
            RESPONSE_BODY
        );

        let captured_request = captured_request.await.expect("captured request");
        assert_eq!(request_body(&captured_request), REQUEST_BODY);
        assert_eq!(
            request_header(&captured_request, "content-type"),
            Some("application/vnd.test+json")
        );
    }

    #[tokio::test]
    async fn non_stream_json_decode_error_retains_upstream_metadata() {
        let (url, _captured_request) = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-request-id: upstream-123\r\ncontent-length: 16\r\n\r\nnot valid json!!",
        )
        .await;
        let client = ProxyClient::new(reqwest::Client::new());

        let err = client
            .call_non_stream(
                &url,
                HeaderMap::new(),
                serde_json::json!({"model": "gemini"}),
            )
            .await
            .expect_err("invalid upstream JSON must fail");

        let decode = err
            .downcast_ref::<UpstreamResponseDecodeError>()
            .expect("decode failure should expose upstream status, headers, and raw body");
        assert_eq!(decode.status, 200);
        assert_eq!(
            decode
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("upstream-123")
        );
        assert_eq!(decode.body_text(), "not valid json!!");
    }

    #[tokio::test]
    async fn http_status_is_not_replayed_as_a_connect_failure() {
        let (url, captured_request) = serve_once(
            b"HTTP/1.1 502 Bad Gateway\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
        )
        .await;
        let client = ProxyClient::new(reqwest::Client::new()).with_connect_retries(1);

        let response = client
            .call_stream_raw(&url, HeaderMap::new(), Bytes::from_static(b"{}"))
            .await
            .expect("an HTTP response is not a connect failure");

        assert_eq!(response.status().as_u16(), 502);
        captured_request
            .await
            .expect("exactly one request was sent");
    }

    #[tokio::test]
    async fn refused_connect_is_not_replayed_without_a_budget() {
        let client = ProxyClient::new(
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(1))
                .build()
                .expect("client"),
        );
        let started = std::time::Instant::now();

        let error = client
            .call_stream_raw(
                "http://127.0.0.1:1/v1/responses",
                HeaderMap::new(),
                Bytes::from_static(b"{}"),
            )
            .await
            .map(|_| ())
            .expect_err("closed port must fail");

        let connect = error
            .downcast_ref::<UpstreamReadError>()
            .expect("connect failure stays an upstream read error");
        assert!(connect.is_transient_connect());
        assert!(
            started.elapsed() < TRANSIENT_CONNECT_RETRY_BACKOFF,
            "a zero budget must not wait for a replay"
        );
    }

    #[tokio::test]
    async fn refused_connect_is_replayed_once_when_the_port_comes_up() {
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve port");
        let addr = reserved.local_addr().expect("reserved addr");
        drop(reserved);

        let url = format!("http://{addr}/v1/responses");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let listener = tokio::net::TcpListener::bind(addr).await.expect("rebind");
            let (mut socket, _) = listener.accept().await.expect("accept replay");
            let _ = read_request(&mut socket).await;
            let body = b"{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write status");
            socket.write_all(body).await.expect("write body");
            socket.shutdown().await.expect("shutdown");
        });

        let client = ProxyClient::new(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("client"),
        )
        .with_connect_retries(1);

        let response = client
            .call_stream_raw(&url, HeaderMap::new(), Bytes::from_static(b"{}"))
            .await
            .expect("the second connect reaches the listener");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.bytes().await.expect("body"), "{}");
    }

    fn retry_provider(vendor: Option<&str>, channel: Option<&str>) -> Provider {
        Provider {
            keys: Vec::new(),
            id: "provider-1".to_string(),
            name: "Probe".to_string(),
            vendor: vendor.map(str::to_string),
            protocol: "devin-chat".to_string(),
            base_url: "https://server.codeium.com".to_string(),
            protocol_mode: "fixed".to_string(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: channel.map(str::to_string),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".to_string(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn only_fake_ip_reset_channels_replay_a_dropped_connect() {
        // Both channels with production `tls handshake eof` evidence.
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("xai"), Some("grok"))),
            1
        );
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("devin"), Some("cli"))),
            1
        );
        // Channel matching is case-insensitive, mirroring vendor checks.
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("Devin"), Some("CLI"))),
            1
        );
        // Wrong channel, missing channel, or a cross combination stays 0.
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("devin"), Some("default"))),
            0
        );
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("devin"), None)),
            0
        );
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("xai"), Some("cli"))),
            0
        );
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("kiro"), Some("cli"))),
            0
        );
        // Other vendors, and vendor-less providers, stay single-attempt.
        assert_eq!(
            transient_connect_retries(&retry_provider(Some("openai"), Some("grok"))),
            0
        );
        assert_eq!(
            transient_connect_retries(&retry_provider(None, Some("grok"))),
            0
        );
        assert_eq!(transient_connect_retries(&retry_provider(None, None)), 0);
    }
}
