use super::{PerformanceMetadata, Terminal, recover_historical_effort};
use crate::logging::LogEntry;
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
    started: Option<Instant>,
    terminal: Terminal,
    upstream_eof: bool,
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
            started: None,
            terminal: Default::default(),
            upstream_eof: false,
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
    pub fn producer(&self) -> Arc<ProducerGuard> {
        self.0.lock().unwrap().producers += 1;
        Arc::new(ProducerGuard(
            self.clone(),
            std::sync::atomic::AtomicBool::new(false),
        ))
    }
    pub fn request(&self, bytes: &[u8]) {
        let mut s = self.0.lock().unwrap();
        s.metadata = recover_historical_effort(&String::from_utf8_lossy(bytes), None);
        s.started = Some(Instant::now());
    }
    pub fn response(&self, status: u16, headers: &reqwest::header::HeaderMap) {
        let mut s = self.0.lock().unwrap();
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
        if status >= 400 {
            s.fail("failed", format!("upstream HTTP {status}"));
        }
    }
    pub fn chunk(&self, bytes: &[u8]) {
        let mut s = self.0.lock().unwrap();
        if s.metadata.first_chunk_ms.is_none() {
            s.metadata.first_chunk_ms = s.started.map(|t| t.elapsed().as_millis() as i64);
        }
        s.terminal.push(bytes);
        if s.terminal.sse {
            s.metadata.response_mode = "stream".into();
        }
    }
    pub fn eof(&self) {
        let mut s = self.0.lock().unwrap();
        if !s.upstream_eof {
            s.upstream_eof = true;
            s.metadata.upstream_duration_ms = s.started.map(|t| t.elapsed().as_millis() as i64);
            s.terminal.finish();
        }
    }
    pub fn fail(&self, state: &str, _reason: impl Into<String>) {
        // Do not persist error chains: reqwest/hook errors may contain URLs,
        // credentials or user content. Original terminal reasons are parsed separately.
        let reason = match state {
            "timed_out" => "upstream_or_pipeline_timeout",
            "cancelled" => "upstream_or_pipeline_cancelled",
            _ => "upstream_or_pipeline_error",
        };
        self.0.lock().unwrap().fail(state, reason.into());
    }
    pub fn log(&self, entry: LogEntry) {
        let mut s = self.0.lock().unwrap();
        if entry.client_status_code >= 400 {
            s.fail(
                "failed",
                format!("client HTTP {}", entry.client_status_code),
            );
        }
        s.entry = Some(entry);
        s.finalize();
    }
    pub fn wrap(&self, response: Response) -> Response {
        self.0.lock().unwrap().delivery_registered = true;
        if response.status().as_u16() >= 400 {
            self.fail("failed", format!("client HTTP {}", response.status()));
        }
        let (parts, body) = response.into_parts();
        let ended = body.is_end_stream();
        if ended {
            self.delivery(true);
        }
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
    fn fail(&mut self, state: &str, reason: String) {
        if matches!(self.metadata.completion.as_str(), "unknown" | "completed") {
            self.metadata.completion = state.into();
            self.metadata.completion_reason = Some(reason);
        }
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
        if self.metadata.completion == "unknown" && self.upstream_eof {
            if let Some(completion) = self.terminal.completion {
                self.metadata.completion = completion.into();
                self.metadata.completion_reason = self.terminal.reason.clone();
            }
        }
        if self.metadata.completion == "completed" && self.delivery == Some(true) {
            self.metadata.completed_at = Some(chrono::Utc::now().timestamp_millis());
        }
        let mut entry = self.entry.take().unwrap();
        entry.performance = self.metadata.clone();
        self.sent = true;
        let _ = self.tx.try_send(entry);
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
            s.fail(
                "failed",
                "response producer exited without final log".into(),
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
        match &result {
            Poll::Ready(Some(Err(error))) => {
                self.attempt
                    .fail("failed", format!("downstream body error: {error}"));
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
        self.body.is_end_stream()
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
                self.attempt.fail(
                    if error.is_timeout() {
                        "timed_out"
                    } else {
                        "failed"
                    },
                    error.to_string(),
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
        assert_eq!(rx.try_recv().unwrap().performance.completion, "failed");
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
            drop(body);
            let log = rx.try_recv().unwrap();
            assert_ne!(log.performance.completion, "completed");
            if eof {
                assert_eq!(log.performance.completion, "incomplete");
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
}
