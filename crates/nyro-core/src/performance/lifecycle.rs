use super::{PerformanceMetadata, Terminal, recover_historical_effort};
use crate::logging::LogEntry;
use crate::logging::diagnostics::{LogDiagnostic, RequestResult};
use crate::logging::payload::{
    BoundedPayloadCapture, CapturedPayload, capture_bytes, capture_headers,
};
use crate::proxy::context::{CancellationToken, Deadline};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::Stream;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Instant,
};

#[derive(Clone)]
pub(crate) struct Attempt(Arc<Mutex<State>>);
struct State {
    metadata: PerformanceMetadata,
    diagnostic: LogDiagnostic,
    final_attempt_count: Option<i32>,
    upstream_capture: BoundedPayloadCapture,
    client_capture: BoundedPayloadCapture,
    request_capture: Option<Arc<CapturedPayload>>,
    upstream_status: Option<u16>,
    client_status: Option<u16>,
    upstream_request_headers: Option<String>,
    upstream_response_headers: Option<String>,
    client_response_headers: Option<String>,
    started: Option<Instant>,
    terminal: Terminal,
    upstream_eof: bool,
    client_frames_sent: u64,
    client_frames_polled: u64,
    delivery: Option<bool>,
    delivery_registered: bool,
    producers: usize,
    entry: Option<LogEntry>,
    sent: bool,
    tx: tokio::sync::mpsc::Sender<LogEntry>,
    cancellation: CancellationToken,
    deadline: Deadline,
}
impl Attempt {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<LogEntry>,
        cancellation: CancellationToken,
        deadline: Deadline,
    ) -> Self {
        Self(Arc::new(Mutex::new(State {
            metadata: Default::default(),
            diagnostic: Default::default(),
            final_attempt_count: None,
            upstream_capture: Default::default(),
            client_capture: Default::default(),
            request_capture: None,
            upstream_status: None,
            client_status: None,
            upstream_request_headers: None,
            upstream_response_headers: None,
            client_response_headers: None,
            started: None,
            terminal: Default::default(),
            upstream_eof: false,
            client_frames_sent: 0,
            client_frames_polled: 0,
            delivery: None,
            delivery_registered: false,
            producers: 0,
            entry: None,
            sent: false,
            tx,
            cancellation,
            deadline,
        })))
    }
    /// Identity is allocated once at construction, before any fallback clones.
    pub fn correlate(&self, request_id: &str, attempt_index: i32) {
        self.with_diagnostic(|d| {
            d.client_request_id = Some(request_id.to_owned());
            d.attempt_index = Some(attempt_index);
        });
    }
    pub fn with_diagnostic(&self, f: impl FnOnce(&mut LogDiagnostic)) {
        f(&mut self.0.lock().unwrap().diagnostic);
    }
    pub fn diagnostic(&self) -> LogDiagnostic {
        self.0.lock().unwrap().diagnostic.clone()
    }
    /// Called only after retry selection. Emission still waits for outer Body EOS.
    pub fn select_final(&self, attempt_count: i32) {
        self.0.lock().unwrap().final_attempt_count = Some(attempt_count);
    }
    pub fn expect_delivery(&self) {
        self.0.lock().unwrap().delivery_registered = true;
    }
    pub fn record_failure(
        &self,
        outcome: &str,
        kind: &str,
        stage: &str,
        error: &(dyn std::error::Error + 'static),
    ) {
        let mut s = self.0.lock().unwrap();
        if s.accepts(outcome) {
            s.diagnostic.record_failure(outcome, kind, stage, error);
            s.sync_diagnostic();
        }
    }
    pub fn record_message(&self, outcome: &str, kind: &str, stage: &str, message: &str) {
        self.0
            .lock()
            .unwrap()
            .record_message(outcome, kind, stage, message);
    }
    /// Relay-side fact: one complete client-facing frame was accepted into the
    /// response-body channel. `DeliveryBody` counts the frames the HTTP layer
    /// actually consumed; equality proves full client delivery even when the
    /// terminal EOS poll loses the race against a client that stops reading
    /// right after the protocol terminal event.
    pub fn note_client_frame_sent(&self) {
        self.0.lock().unwrap().client_frames_sent += 1;
    }
    pub fn producer(&self) -> Arc<ProducerGuard> {
        self.0.lock().unwrap().producers += 1;
        Arc::new(ProducerGuard(
            self.clone(),
            std::sync::atomic::AtomicBool::new(false),
        ))
    }
    pub fn request(&self, bytes: &[u8]) {
        let mut s = self.0.lock().unwrap();
        s.request_capture = Some(Arc::new(capture_bytes(bytes, true)));
        s.metadata = if bytes.len() <= 1024 * 1024 {
            recover_historical_effort(&String::from_utf8_lossy(bytes), None)
        } else {
            Default::default()
        };
        s.started = Some(Instant::now());
    }
    pub fn request_headers(&self, headers: &reqwest::header::HeaderMap) {
        let captured = capture_headers(headers);
        let mut s = self.0.lock().unwrap();
        s.diagnostic.payload_metadata["upstream_request_headers"] = captured.metadata;
        s.upstream_request_headers = captured.headers;
    }
    pub fn response(&self, status: u16, headers: &reqwest::header::HeaderMap) {
        let mut s = self.0.lock().unwrap();
        s.upstream_status = Some(status);
        let captured = capture_headers(headers);
        s.diagnostic.payload_metadata["upstream_response_headers"] = captured.metadata;
        s.upstream_response_headers = captured.headers;
        s.metadata.response_mode = if headers
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/event-stream"))
        {
            "stream"
        } else {
            "buffered"
        }
        .into();
        if (400..600).contains(&status) {
            s.fail("failed", format!("upstream HTTP {status}"));
        }
    }
    pub fn chunk(&self, bytes: &[u8]) {
        let mut s = self.0.lock().unwrap();
        if s.metadata.first_chunk_ms.is_none() {
            s.metadata.first_chunk_ms = s.started.map(|t| t.elapsed().as_millis() as i64);
        }
        s.upstream_capture.push(bytes);
        s.terminal.push(bytes);
        // Explicit upstream failure survives a later client disconnect/EOF loss.
        if s.terminal.completion == Some("failed") {
            let reason = s
                .terminal
                .error_message
                .clone()
                .or_else(|| s.terminal.reason.clone())
                .unwrap_or_else(|| "Upstream error event.".into());
            s.record_message("failed", "upstream_error", "upstream_response", &reason);
        }
        if s.terminal.sse {
            s.metadata.response_mode = "stream".into();
        }
    }
    pub fn eof(&self) {
        let mut s = self.0.lock().unwrap();
        if !s.upstream_eof {
            s.upstream_eof = true;
            s.upstream_capture.push(&[]);
            s.metadata.upstream_duration_ms = s.started.map(|t| t.elapsed().as_millis() as i64);
            s.terminal.finish();
        }
    }
    pub fn fail(&self, state: &str, reason: impl Into<String>) {
        let kind = match state {
            "timed_out" => "timeout",
            "cancelled" => "client_cancelled",
            _ => "pipeline_error",
        };
        self.record_message(state, kind, "response", &reason.into());
    }
    pub fn log(&self, entry: LogEntry) {
        let mut s = self.0.lock().unwrap();
        if (400..600).contains(&entry.client_status_code) {
            s.fail(
                "failed",
                format!("client HTTP {}", entry.client_status_code),
            );
        }
        if let Some(metadata) = entry.diagnostic.payload_metadata.as_object() {
            for (key, value) in metadata {
                s.diagnostic.payload_metadata[key] = value.clone();
            }
        }
        s.entry = Some(entry);
        s.finalize();
    }
    pub fn wrap(&self, response: Response) -> Response {
        self.0.lock().unwrap().delivery_registered = true;
        if (400..600).contains(&response.status().as_u16()) {
            self.fail("failed", format!("client HTTP {}", response.status()));
        }
        let (mut parts, body) = response.into_parts();
        parts.extensions.insert(self.clone());
        {
            let mut s = self.0.lock().unwrap();
            s.client_status = Some(parts.status.as_u16());
            let captured = capture_headers(&parts.headers);
            s.diagnostic.payload_metadata["client_response_headers"] = captured.metadata;
            s.client_response_headers = captured.headers;
        }
        // Even empty bodies wait until selected as the final response. Selection
        // occurs outside the retry loop before this wrapper can be consumed.
        let ended = false;
        Response::from_parts(
            parts,
            Body::new(DeliveryBody {
                body,
                attempt: self.clone(),
                ended,
            }),
        )
    }
    fn delivery(&self, clean: bool) {
        let mut s = self.0.lock().unwrap();
        if s.delivery.is_none() {
            s.delivery = Some(clean);
            if clean {
                s.client_capture.push(&[]);
            }
        }
        if !clean {
            s.fail("cancelled", "downstream body dropped before EOS".into());
        }
        s.finalize();
    }
    pub fn observe<S>(&self, stream: S) -> ObservedStream<S> {
        ObservedStream {
            inner: Box::pin(stream),
            attempt: self.clone(),
            ended: false,
        }
    }
}
impl State {
    /// Every frame the relay produced for the client body was consumed by the
    /// HTTP layer. Frames flow one-to-one through the response-body channel,
    /// so `polled >= sent > 0` means the client received the complete stream
    /// content even though the terminal EOS poll may never have happened.
    fn client_drained(&self) -> bool {
        self.client_frames_sent > 0 && self.client_frames_polled >= self.client_frames_sent
    }
    fn accepts(&self, outcome: &str) -> bool {
        fn rank(outcome: &str) -> u8 {
            match outcome {
                "failed" | "timed_out" => 5,
                "cancelled" => 4,
                "output_limited" => 3,
                "unknown" => 1,
                _ => 0,
            }
        }
        self.diagnostic.outcome_version == 0
            || rank(outcome) > rank(&self.diagnostic.attempt_outcome)
    }
    fn sync_diagnostic(&mut self) {
        self.metadata.completion = self.diagnostic.attempt_outcome.clone();
        self.metadata.completion_reason = self.diagnostic.failure_kind.clone();
    }
    fn record_message(&mut self, outcome: &str, kind: &str, stage: &str, message: &str) {
        if self.accepts(outcome) {
            self.diagnostic
                .record_message(outcome, kind, stage, message);
            self.sync_diagnostic();
        }
    }
    fn fail(&mut self, state: &str, reason: String) {
        let (kind, stage) = match state {
            "timed_out" => ("timeout", "upstream_read"),
            "cancelled" => ("client_cancelled", "client_delivery"),
            _ if reason.starts_with("upstream HTTP") => {
                ("upstream_http_error", "upstream_response")
            }
            _ if reason.starts_with("client HTTP") => ("http_error", "response"),
            _ => ("pipeline_error", "response"),
        };
        self.record_message(state, kind, stage, &reason);
    }
    fn finalize(&mut self) {
        if self.sent || self.producers != 0 || self.delivery.is_none() || self.entry.is_none() {
            return;
        }
        if self.deadline.is_exceeded() {
            self.fail("timed_out", "request deadline exceeded".into());
        }
        if self.cancellation.is_cancelled() {
            self.fail("cancelled", "request cancelled".into());
        }
        if self.upstream_eof {
            if let Some(completion) = self.terminal.completion {
                let reason = self.terminal.reason.clone().unwrap_or_default();
                match completion {
                    "failed" | "cancelled" | "output_limited" => {
                        let kind = match completion {
                            "failed" if reason == "missing_terminal" => "missing_terminal",
                            "failed" => "upstream_error",
                            "cancelled" => "upstream_cancelled",
                            _ => "output_limit",
                        };
                        self.record_message(completion, kind, "upstream_response", &reason);
                    }
                    "unknown" => {
                        self.record_message("unknown", "incomplete", "upstream_response", &reason)
                    }
                    "completed"
                        if self.diagnostic.outcome_version == 0
                            && (self.delivery == Some(true) || self.client_drained()) =>
                    {
                        self.diagnostic.outcome_version = 1;
                        self.diagnostic.attempt_outcome = "completed".into();
                        self.metadata.completion = "completed".into();
                        self.metadata.completion_reason = Some(reason);
                    }
                    _ => {}
                }
            }
        }
        // SSE clients (codex CLI and similar) may close the connection the
        // moment they read the protocol terminal event — nothing follows it,
        // so waiting for more bytes would only stall. The disconnect then wins
        // two harmless races: this response body is dropped before its
        // terminal EOS poll (recorded as cancelled by `DeliveryBody`) and the
        // upstream stream is dropped before its EOF poll. An unambiguous
        // upstream completed terminal plus proof that the HTTP layer consumed
        // every produced client frame reconciles those drop artifacts into
        // confirmed completion. Confirmed failures, timeouts and output
        // limits stay sticky and are never demoted.
        if self.terminal.confirmed_completed()
            && self.client_drained()
            && !matches!(
                self.diagnostic.attempt_outcome.as_str(),
                "failed" | "timed_out" | "completed" | "output_limited"
            )
        {
            self.diagnostic.outcome_version = crate::logging::diagnostics::OUTCOME_VERSION;
            self.diagnostic.attempt_outcome = "completed".into();
            self.diagnostic.failure_kind = None;
            self.diagnostic.failure_stage = None;
            self.diagnostic.error_message = None;
            self.diagnostic.error_causes = Vec::new();
            self.metadata.completion = "completed".into();
            self.metadata.completion_reason = self.terminal.reason.clone();
        }
        let finished_at = chrono::Utc::now().timestamp_millis();
        if self.metadata.completion == "completed"
            && (self.delivery == Some(true) || self.client_drained())
        {
            self.metadata.completed_at = Some(finished_at);
        }
        if let (Some(attempt_count), Some(request_id)) = (
            self.final_attempt_count,
            self.diagnostic.client_request_id.clone(),
        ) {
            self.diagnostic.final_result = Some(RequestResult {
                client_request_id: request_id,
                final_outcome: self.diagnostic.attempt_outcome.clone(),
                final_attempt_id: Some(self.diagnostic.log_id.clone()),
                attempt_count,
                finished_at,
            });
        }
        let mut entry = self.entry.take().unwrap();
        let upstream = std::mem::take(&mut self.upstream_capture).finish(self.upstream_eof);
        let client = std::mem::take(&mut self.client_capture)
            .finish(self.delivery == Some(true) || self.client_drained());
        entry.upstream_response_body = upstream.body;
        entry.client_response_body = client.body;
        if let Some(status) = self.upstream_status {
            entry.upstream_status_code = Some(status as i32);
        }
        if let Some(status) = self.client_status {
            entry.client_status_code = status as i32;
        }
        entry.upstream_request_headers = self
            .upstream_request_headers
            .take()
            .or(entry.upstream_request_headers);
        entry.upstream_response_headers = self
            .upstream_response_headers
            .take()
            .or(entry.upstream_response_headers);
        entry.client_response_headers = self
            .client_response_headers
            .take()
            .or(entry.client_response_headers);
        self.diagnostic.payload_metadata["upstream_response_body"] = upstream.metadata;
        self.diagnostic.payload_metadata["client_response_body"] = client.metadata;
        if let Some(request) = self.request_capture.take() {
            match Arc::try_unwrap(request) {
                Ok(request) => {
                    entry.upstream_request_body = request.body;
                    self.diagnostic.payload_metadata["upstream_request_body"] = request.metadata;
                }
                Err(request) => {
                    entry.upstream_request_body = request.body.clone();
                    self.diagnostic.payload_metadata["upstream_request_body"] =
                        request.metadata.clone();
                }
            }
        }
        entry.performance = self.metadata.clone();
        entry.diagnostic = self.diagnostic.clone();
        self.sent = true;
        crate::logging::enqueue_log(&self.tx, entry);
    }
}
pub(crate) struct ProducerGuard(Attempt, std::sync::atomic::AtomicBool);
impl ProducerGuard {
    pub fn finish(&self) {
        self.1.store(true, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for ProducerGuard {
    fn drop(&mut self) {
        let mut s = self.0.0.lock().unwrap();
        if !self.1.load(std::sync::atomic::Ordering::Acquire) {
            // Dropping a producer is not evidence of an upstream fault. A known
            // transport/parser failure has already been recorded and is sticky.
            s.fail(
                "cancelled",
                "response producer dropped before completion".into(),
            );
        }
        s.producers -= 1;
        if s.producers == 0 && !s.delivery_registered {
            s.delivery = Some(false);
            s.fail(
                "cancelled",
                "request producer dropped before response delivery".into(),
            );
        }
        s.finalize();
    }
}
struct DeliveryBody {
    body: Body,
    attempt: Attempt,
    ended: bool,
}
impl HttpBody for DeliveryBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &result {
            if let Some(bytes) = frame.data_ref() {
                let mut s = self.attempt.0.lock().unwrap();
                s.client_frames_polled += 1;
                s.client_capture.push(bytes);
            }
        }
        match &result {
            Poll::Ready(Some(Err(error))) => {
                self.attempt
                    .record_failure("failed", "body_read_error", "client_delivery", error);
                self.ended = true;
                self.attempt.delivery(false);
            }
            Poll::Ready(None) => {
                self.ended = true;
                self.attempt.delivery(true);
            }
            Poll::Ready(Some(Ok(_))) if self.body.is_end_stream() => {
                self.ended = true;
                self.attempt.delivery(true);
            }
            _ => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.ended
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
impl Drop for DeliveryBody {
    fn drop(&mut self) {
        if !self.ended {
            self.attempt.delivery(false);
        }
    }
}
pub(crate) struct ObservedStream<S> {
    inner: Pin<Box<S>>,
    attempt: Attempt,
    ended: bool,
}
impl<S: Stream<Item = Result<Bytes, reqwest::Error>>> Stream for ObservedStream<S> {
    type Item = Result<Bytes, reqwest::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.inner.as_mut().poll_next(cx);
        match &result {
            Poll::Ready(Some(Ok(bytes))) => self.attempt.chunk(bytes),
            Poll::Ready(Some(Err(error))) => {
                self.attempt.record_failure(
                    if error.is_timeout() {
                        "timed_out"
                    } else {
                        "failed"
                    },
                    if error.is_timeout() {
                        "timeout"
                    } else {
                        "upstream_read_error"
                    },
                    "upstream_read",
                    error,
                );
                self.ended = true;
            }
            Poll::Ready(None) => {
                self.attempt.eof();
                self.ended = true;
            }
            _ => {}
        }
        result
    }
}
impl<S> Drop for ObservedStream<S> {
    fn drop(&mut self) {
        if !self.ended {
            self.attempt
                .fail("cancelled", "original upstream body not read to EOF");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry() -> LogEntry {
        LogEntry {
            diagnostic: Default::default(),
            performance: Default::default(),
            api_key_id: None,
            api_key_name: None,
            created_at: 0,
            client_protocol: String::new(),
            upstream_protocol: String::new(),
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: None,
            model_name: None,
            upstream_url: None,
            client_model: String::new(),
            upstream_model: String::new(),
            reasoning_effort: None,
            route_decision: None,
            method: None,
            path: None,
            client_request_headers: None,
            client_request_body: None,
            client_response_headers: None,
            client_response_body: None,
            upstream_request_headers: None,
            upstream_request_body: None,
            upstream_response_headers: None,
            upstream_response_body: None,
            upstream_status_code: Some(200),
            client_status_code: 200,
            latency_total_ms: 0,
            latency_upstream_ms: None,
            usage: Default::default(),
            is_stream: true,
            stream_chunks_count: 0,
            stream_first_chunk_ms: None,
            enable_payload: None,
        }
    }
    fn setup() -> (
        Attempt,
        Arc<ProducerGuard>,
        tokio::sync::mpsc::Receiver<LogEntry>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let a = Attempt::new(tx, CancellationToken::new(), Deadline::never());
        let g = a.producer();
        a.log(entry());
        a.request(br#"{"reasoning_effort":"high"}"#);
        (a, g, rx)
    }
    fn upstream_done(a: &Attempt) {
        a.chunk(
            b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        a.eof();
    }
    async fn frame(body: &mut Body) -> Option<Result<Frame<Bytes>, axum::Error>> {
        futures::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }
    #[tokio::test]
    async fn final_frame_eos_is_delivery_without_extra_none_poll() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        let mut body = a.wrap(Response::new(Body::from("done"))).into_body();
        assert_eq!(body.size_hint().exact(), Some(4));
        g.finish();
        drop(g);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            frame(&mut body)
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap(),
            "done"
        );
        let log = rx.try_recv().unwrap();
        assert_eq!(log.performance.completion, "completed");
        assert!(log.performance.completed_at.is_some());
        drop(body);
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn enqueue_or_drop_never_counts_as_delivery() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        let body = a.wrap(Response::new(Body::from("queued but not polled")));
        g.finish();
        drop(g);
        assert!(rx.try_recv().is_err());
        drop(body);
        let log = rx.try_recv().unwrap();
        assert_eq!(log.performance.completion, "cancelled");
        assert!(log.performance.completed_at.is_none());
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn delivery_before_producer_waits_and_error_is_sticky() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        let mut body = a.wrap(Response::new(Body::from("done"))).into_body();
        frame(&mut body).await.unwrap().unwrap();
        assert!(rx.try_recv().is_err());
        a.fail("failed", "hook rejected after terminal");
        g.finish();
        drop(g);
        assert_eq!(rx.try_recv().unwrap().performance.completion, "failed");
    }
    #[tokio::test]
    async fn producer_abort_still_logs_once() {
        let (a, g, mut rx) = setup();
        let body = a.wrap(Response::new(Body::from("pending")));
        drop(g);
        drop(body);
        assert_eq!(rx.try_recv().unwrap().performance.completion, "cancelled");
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn completion_requires_original_eof_and_excludes_token_limits() {
        for eof in [false, true] {
            let (a, g, mut rx) = setup();
            a.chunk(b"data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n");
            if eof {
                a.eof();
            }
            let body = a.wrap(Response::new(Body::empty()));
            g.finish();
            drop(g);
            axum::body::to_bytes(body.into_body(), 100).await.unwrap();
            let log = rx.try_recv().unwrap();
            assert_ne!(log.performance.completion, "completed");
            if eof {
                assert_eq!(log.performance.completion, "unknown");
                assert_eq!(log.diagnostic.failure_kind.as_deref(), Some("incomplete"));
            }
        }
    }
    #[tokio::test]
    async fn duration_freezes_at_upstream_eof_not_drain() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        let duration = a.0.lock().unwrap().metadata.upstream_duration_ms;
        let mut body = a.wrap(Response::new(Body::from("done"))).into_body();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        frame(&mut body).await.unwrap().unwrap();
        g.finish();
        drop(g);
        assert_eq!(
            rx.try_recv().unwrap().performance.upstream_duration_ms,
            duration
        );
    }
    struct TrailersBody {
        stage: u8,
    }
    impl HttpBody for TrailersBody {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            let result = match self.stage {
                0 => Some(Ok(Frame::data(Bytes::from_static(b"x")))),
                1 => {
                    let mut h = axum::http::HeaderMap::new();
                    h.insert("x-trailer", "yes".parse().unwrap());
                    Some(Ok(Frame::trailers(h)))
                }
                _ => None,
            };
            self.stage += 1;
            Poll::Ready(result)
        }
        fn is_end_stream(&self) -> bool {
            self.stage >= 2
        }
        fn size_hint(&self) -> SizeHint {
            SizeHint::with_exact(if self.stage == 0 { 1 } else { 0 })
        }
    }
    #[tokio::test]
    async fn preserves_trailers_and_requires_the_last_frame() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        let mut body = a
            .wrap(Response::new(Body::new(TrailersBody { stage: 0 })))
            .into_body();
        assert_eq!(body.size_hint().exact(), Some(1));
        g.finish();
        drop(g);
        assert!(frame(&mut body).await.unwrap().unwrap().is_data());
        assert!(rx.try_recv().is_err());
        let trailers = frame(&mut body)
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap();
        assert_eq!(trailers["x-trailer"], "yes");
        assert_eq!(rx.try_recv().unwrap().performance.completion, "completed");
    }
    #[tokio::test]
    async fn none_eos_and_body_errors_are_distinct() {
        for error in [false, true] {
            let (a, g, mut rx) = setup();
            upstream_done(&a);
            let stream = futures::stream::iter(if error {
                vec![Err(std::io::Error::other("fault"))]
            } else {
                vec![Ok(Bytes::from_static(b"x"))]
            });
            let mut body = a.wrap(Response::new(Body::from_stream(stream))).into_body();
            g.finish();
            drop(g);
            if error {
                assert!(frame(&mut body).await.unwrap().is_err());
                assert_eq!(rx.try_recv().unwrap().performance.completion, "failed");
            } else {
                frame(&mut body).await.unwrap().unwrap();
                assert!(rx.try_recv().is_err());
                assert!(frame(&mut body).await.is_none());
                assert_eq!(rx.try_recv().unwrap().performance.completion, "completed");
            }
        }
    }
    #[tokio::test]
    async fn cancelled_and_timed_out_context_never_promotes() {
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        a.0.lock().unwrap().cancellation.cancel();
        let body = a.wrap(Response::new(Body::empty()));
        g.finish();
        drop(g);
        drop(body);
        assert_eq!(rx.try_recv().unwrap().performance.completion, "cancelled");
        let (a, g, mut rx) = setup();
        upstream_done(&a);
        a.0.lock().unwrap().deadline = Deadline::from_now(std::time::Duration::ZERO);
        let body = a.wrap(Response::new(Body::empty()));
        g.finish();
        drop(g);
        drop(body);
        assert_eq!(rx.try_recv().unwrap().performance.completion, "timed_out");
    }
    #[tokio::test]
    async fn terminal_delivered_client_close_is_completed_not_cancelled() {
        // codex-style consumers stop reading once they see the protocol
        // terminal event and close the connection. Both the upstream EOF poll
        // and the response-body EOS poll then lose the race against that
        // close. An unambiguous upstream terminal plus proof that the HTTP
        // layer consumed every produced client frame reconciles the resulting
        // drop artifacts into confirmed completion.
        let (a, g, mut rx) = setup();
        // Upstream bytes including the terminal arrive, but the upstream
        // stream is dropped before its EOF poll (no a.eof()).
        a.chunk(
            b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        a.note_client_frame_sent();
        let mut body = a
            .wrap(Response::new(Body::from_stream(futures::stream::iter(
                vec![Ok::<_, std::io::Error>(Bytes::from_static(
                    b"data: done\n\n",
                ))],
            ))))
            .into_body();
        g.finish();
        drop(g);
        frame(&mut body).await.unwrap().unwrap();
        drop(body); // client closes right after the terminal, before the EOS poll
        let log = rx.try_recv().unwrap();
        assert_eq!(log.diagnostic.outcome_version, 1);
        assert_eq!(log.diagnostic.attempt_outcome, "completed");
        assert_eq!(log.performance.completion, "completed");
        assert!(log.performance.completed_at.is_some());
        assert!(log.diagnostic.failure_kind.is_none());
        assert!(log.diagnostic.error_message.is_none());
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn undrained_client_frames_stay_cancelled_after_terminal() {
        // A client that disconnects before the relay's frames were fully
        // consumed by the HTTP layer is a genuine mid-stream cancellation.
        let (a, g, mut rx) = setup();
        a.chunk(
            b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        a.note_client_frame_sent();
        a.note_client_frame_sent();
        let mut body = a
            .wrap(Response::new(Body::from_stream(futures::stream::iter(
                vec![
                    Ok::<_, std::io::Error>(Bytes::from_static(b"first\n\n")),
                    Ok::<_, std::io::Error>(Bytes::from_static(b"second\n\n")),
                ],
            ))))
            .into_body();
        g.finish();
        drop(g);
        frame(&mut body).await.unwrap().unwrap(); // only the first frame delivered
        drop(body); // mid-stream disconnect
        let log = rx.try_recv().unwrap();
        assert_eq!(log.diagnostic.attempt_outcome, "cancelled");
        assert_eq!(
            log.diagnostic.failure_kind.as_deref(),
            Some("client_cancelled")
        );
        assert!(log.performance.completed_at.is_none());
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn confirmed_failure_survives_full_client_drain() {
        // Full delivery of an upstream error stream must stay failed; drain
        // evidence never demotes a confirmed failure.
        let (a, g, mut rx) = setup();
        a.chunk(b"data: {\"error\":{\"message\":\"boom\"}}\n\ndata: [DONE]\n\n");
        a.note_client_frame_sent();
        let mut body = a
            .wrap(Response::new(Body::from_stream(futures::stream::iter(
                vec![Ok::<_, std::io::Error>(Bytes::from_static(
                    b"data: {\"error\":{}}\n\n",
                ))],
            ))))
            .into_body();
        g.finish();
        drop(g);
        frame(&mut body).await.unwrap().unwrap();
        drop(body);
        let log = rx.try_recv().unwrap();
        assert_eq!(log.diagnostic.attempt_outcome, "failed");
        assert_eq!(
            log.diagnostic.failure_kind.as_deref(),
            Some("upstream_error")
        );
        assert!(rx.try_recv().is_err());
    }
}
