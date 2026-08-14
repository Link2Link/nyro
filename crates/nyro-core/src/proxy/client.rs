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

pub const MAX_UPSTREAM_RESPONSE_BODY_BYTES: usize = 128 * 1024 * 1024;

pub struct ProxyClient {
    pub http: reqwest::Client,
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
        Self { http }
    }

    pub async fn call_non_stream_raw(
        &self,
        url: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<RawUpstreamResponse> {
        ensure_json_content_type(&mut headers);
        let response = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        Self::buffer_response(response).await
    }

    pub async fn buffer_response(response: reqwest::Response) -> Result<RawUpstreamResponse> {
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
            .http
            .post(url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.bytes().await?;
        let json: Value = match serde_json::from_slice(&body) {
            Ok(json) => json,
            Err(source) => {
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
    ) -> Result<reqwest::Response> {
        ensure_json_content_type(&mut headers);
        Ok(self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?)
    }

    pub async fn call_stream(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Value,
    ) -> Result<(reqwest::Response, u16)> {
        let resp = self
            .call_stream_raw(url, headers, serde_json::to_vec(&body)?.into())
            .await?;
        let status = resp.status().as_u16();
        Ok((resp, status))
    }
}

async fn read_body_with_limit(response: reqwest::Response, limit: usize) -> Result<Bytes> {
    let mut stream = response.bytes_stream();
    let mut body = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > limit {
            anyhow::bail!("upstream response body exceeds {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
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
}
