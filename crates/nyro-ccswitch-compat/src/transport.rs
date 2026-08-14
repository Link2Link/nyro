use bytes::Bytes;

/// Header entry represented without depending on an HTTP framework.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: Bytes,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<Bytes>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Classification of an upstream response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    Json,
    Sse,
    Empty,
    Other,
}

/// HTTP-adjacent metadata carried across the core boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseMetadata {
    pub status: u16,
    pub headers: Vec<Header>,
    pub content_type: Option<String>,
}

impl ResponseMetadata {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            content_type: None,
        }
    }

    pub fn body_kind(&self, body: &[u8]) -> BodyKind {
        classify_body(self.content_type.as_deref(), body)
    }

    pub fn rebuilt(mut self, content_type: &'static str) -> Self {
        self.headers.retain(|header| {
            !matches!(
                header.name.to_ascii_lowercase().as_str(),
                "content-length" | "content-encoding" | "transfer-encoding" | "content-type"
            )
        });
        self.content_type = Some(content_type.to_string());
        self
    }
}

const HOP_BY_HOP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Remove standard hop-by-hop headers and extensions named by Connection.
pub fn strip_hop_by_hop_headers(headers: &mut Vec<Header>) {
    let mut connection_listed = Vec::new();
    for header in headers.iter() {
        if header.name.eq_ignore_ascii_case("connection")
            && let Ok(value) = std::str::from_utf8(&header.value)
        {
            connection_listed.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_ascii_lowercase),
            );
        }
    }
    headers.retain(|header| {
        let name = header.name.to_ascii_lowercase();
        !HOP_BY_HOP_RESPONSE_HEADERS.contains(&name.as_str())
            && !connection_listed.iter().any(|listed| listed == &name)
    });
}

pub fn classify_body(content_type: Option<&str>, body: &[u8]) -> BodyKind {
    if body.is_empty() {
        return BodyKind::Empty;
    }
    if content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
    }) {
        return BodyKind::Sse;
    }
    let text = String::from_utf8_lossy(body);
    if body_looks_like_sse(&text) {
        return BodyKind::Sse;
    }
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        BodyKind::Json
    } else {
        BodyKind::Other
    }
}

pub fn body_looks_like_sse(body: &str) -> bool {
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    ["data:", "event:", "id:", "retry:", ":"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// Whether the upstream request must be sent with `Accept-Encoding: identity`.
/// Streaming responses cannot be re-encoded mid-flight, so any request that
/// will produce an SSE stream (body `stream` flag, Gemini `:streamGenerateContent`
/// / `alt=sse` endpoint, or an `Accept: text/event-stream` header) opts out of
/// automatic compression. Non-streaming requests keep automatic compression so
/// the ported bounded decompressor can decode the response.
pub fn should_force_identity_encoding(
    endpoint: &str,
    body_stream: bool,
    accept_header: Option<&str>,
) -> bool {
    if body_stream {
        return true;
    }
    if endpoint.contains("streamGenerateContent") || endpoint.contains("alt=sse") {
        return true;
    }
    accept_header
        .map(|accept| accept.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Classify an upstream body into a finite set of diagnostic categories,
/// preserving HTML/SSE/binary clues without ever logging the content itself.
pub fn classify_body_for_diagnostics(body: &str) -> &'static str {
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    if trimmed.is_empty() {
        return "empty";
    }
    if body_looks_like_sse(trimmed) {
        return "sse";
    }

    // Classification only inspects the first 4 KiB to avoid a second linear
    // scan of an oversized error body just for diagnostics.
    let sample = trimmed.chars().take(4096).collect::<String>();
    let prefix = sample
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase();
    if ["<!doctype html", "<html", "<head", "<body"]
        .iter()
        .any(|marker| prefix.starts_with(marker))
    {
        return "html";
    }
    if sample.contains('\u{fffd}')
        || sample
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return "binary-or-encoded";
    }
    if prefix.starts_with('{') || prefix.starts_with('[') {
        return "json-like";
    }
    "text"
}

/// Field-diagnostics suffix for upstream parse errors: content-type,
/// content-encoding, body length, and a safe classification — never the
/// content itself.
pub fn body_diagnostics_suffix(
    content_type: Option<&str>,
    content_encoding: Option<&str>,
    body: &str,
) -> String {
    format!(
        "(content-type: {}; content-encoding: {}; body-bytes: {}; body-kind: {}; content omitted)",
        content_type.unwrap_or("<none>"),
        content_encoding.unwrap_or("<none>"),
        body.len(),
        classify_body_for_diagnostics(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_looks_like_sse_detects_unlabeled_sse_prefixes() {
        assert!(body_looks_like_sse("data: {\"id\":\"1\"}\n\n"));
        assert!(body_looks_like_sse("event: message\ndata: {}\n\n"));
        assert!(body_looks_like_sse("id: 1\ndata: {}\n\n"));
        assert!(body_looks_like_sse("retry: 3000\ndata: {}\n\n"));
        assert!(body_looks_like_sse(
            ": OPENROUTER PROCESSING\n\ndata: {}\n\n"
        ));
        assert!(body_looks_like_sse("\u{feff}\n  data: {}\n\n"));
        assert!(!body_looks_like_sse("<html><body>blocked</body></html>"));
        assert!(!body_looks_like_sse("Bad Gateway"));
        assert!(!body_looks_like_sse(""));
    }

    #[test]
    fn rebuilt_metadata_removes_stale_entity_headers_only() {
        let mut metadata = ResponseMetadata::new(200);
        metadata.headers = vec![
            Header::new("content-length", Bytes::from_static(b"42")),
            Header::new("x-request-id", Bytes::from_static(b"abc")),
        ];
        let rebuilt = metadata.rebuilt("application/json");
        assert_eq!(rebuilt.headers.len(), 1);
        assert_eq!(rebuilt.headers[0].name, "x-request-id");
    }

    #[test]
    fn test_strip_hop_by_hop_response_headers_removes_standard_headers() {
        let mut headers = vec![
            Header::new("connection", Bytes::from_static(b"keep-alive")),
            Header::new("keep-alive", Bytes::from_static(b"timeout=5")),
            Header::new("transfer-encoding", Bytes::from_static(b"chunked")),
            Header::new("proxy-connection", Bytes::from_static(b"keep-alive")),
            Header::new("content-type", Bytes::from_static(b"application/json")),
            Header::new("content-length", Bytes::from_static(b"12")),
        ];
        strip_hop_by_hop_headers(&mut headers);
        assert_eq!(headers.len(), 2);
        assert!(headers.iter().any(|header| header.name == "content-type"));
        assert!(headers.iter().any(|header| header.name == "content-length"));
    }

    #[test]
    fn test_strip_hop_by_hop_response_headers_removes_connection_listed_extensions() {
        let mut headers = vec![
            Header::new(
                "connection",
                Bytes::from_static(b"x-trace-hop, x-debug-hop, upgrade"),
            ),
            Header::new("x-trace-hop", Bytes::from_static(b"trace")),
            Header::new("x-debug-hop", Bytes::from_static(b"debug")),
            Header::new("upgrade", Bytes::from_static(b"websocket")),
            Header::new("content-type", Bytes::from_static(b"text/event-stream")),
        ];
        strip_hop_by_hop_headers(&mut headers);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name, "content-type");
    }

    #[test]
    fn test_strip_sse_field_accepts_optional_space() {
        assert_eq!(
            crate::ported::sse::strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            crate::ported::sse::strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            crate::ported::sse::strip_sse_field("event:message_start", "event"),
            Some("message_start")
        );
        assert_eq!(crate::ported::sse::strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn force_identity_for_stream_flag_requests() {
        assert!(should_force_identity_encoding("/v1/responses", true, None));
    }

    #[test]
    fn force_identity_for_gemini_stream_endpoints() {
        assert!(should_force_identity_encoding(
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            false,
            None,
        ));
    }

    #[test]
    fn force_identity_for_sse_accept_header() {
        assert!(should_force_identity_encoding(
            "/v1/responses",
            false,
            Some("text/event-stream"),
        ));
    }

    #[test]
    fn non_streaming_requests_allow_automatic_compression() {
        assert!(!should_force_identity_encoding(
            "/v1/responses",
            false,
            None,
        ));
        assert!(!should_force_identity_encoding(
            "/v1/responses",
            false,
            Some("application/json"),
        ));
    }

    #[test]
    fn body_diagnostics_classifies_without_exposing_content() {
        assert_eq!(classify_body_for_diagnostics(""), "empty");
        assert_eq!(classify_body_for_diagnostics("  <HTML>blocked"), "html");
        assert_eq!(classify_body_for_diagnostics("data: {}\n\n"), "sse");
        assert_eq!(classify_body_for_diagnostics("{\"ok\":true}"), "json-like");
        assert_eq!(
            classify_body_for_diagnostics("decoded\u{fffd}payload"),
            "binary-or-encoded"
        );
        assert_eq!(classify_body_for_diagnostics("Bad Gateway"), "text");
    }

    #[test]
    fn body_diagnostics_suffix_carries_field_diagnostics_without_content() {
        let suffix =
            body_diagnostics_suffix(Some("text/html"), Some("gzip"), "<html>\nblocked</html>");
        assert!(suffix.contains("content-type: text/html"), "{suffix}");
        assert!(suffix.contains("content-encoding: gzip"), "{suffix}");
        assert!(suffix.contains("body-bytes: 21"), "{suffix}");
        assert!(suffix.contains("body-kind: html"), "{suffix}");
        assert!(!suffix.contains("blocked"), "{suffix}");
    }

    #[test]
    fn body_diagnostics_suffix_marks_missing_headers() {
        let suffix = body_diagnostics_suffix(None, None, "data: oops");
        assert!(suffix.contains("content-type: <none>"), "{suffix}");
        assert!(suffix.contains("content-encoding: <none>"), "{suffix}");
        assert!(suffix.contains("body-kind: sse"), "{suffix}");
    }
}
