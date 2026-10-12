//! Real HTTP/2 wire tests: no provider, credentials, TLS override, or external network.
//!
//! Cleartext loopback deliberately uses a test-supplied prior-knowledge client.
//! RequestBuilder::version(HTTP_2) alone does not enable h2c in reqwest.

use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use h2::server::SendResponse;
use http::{HeaderMap, HeaderValue, Request, Response, Version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

use super::{ObservedDuplexResponse, UploadStream, UpstreamFrame};
use crate::config::UpstreamTimeouts;
use crate::proxy::client::ProxyClient;
use crate::proxy::context::{CancellationToken, Deadline};

const WAIT: Duration = Duration::from_secs(5);
const UPLOAD_CAPACITY: usize = 1;
const INITIAL: &[u8] = b"synthetic-initial-frame";
const FOLLOW_UP: &[u8] = b"synthetic-tool-result-frame";
const CHALLENGE: &[u8] = b"synthetic-tool-request-frame";
const COMPLETE: &[u8] = b"synthetic-completion-frame";

type UploadItem = Result<Bytes, io::Error>;

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("loopback operation or worker cleanup did not finish within five seconds")
}

/// The only spawned test task is owned, joined on success, and aborted on panic.
/// Shutdown is requested only AFTER wire/stream-release assertions, so server
/// teardown cannot masquerade as client cancellation.
struct LoopbackServer {
    url: String,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    async fn raw<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopped) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let (socket, peer) = listener.accept().await.unwrap();
            assert!(peer.ip().is_loopback());
            tokio::select! {
                _ = stopped => {},
                _ = serve(socket) => {},
            }
        });
        Self {
            url: format!("http://{address}/synthetic-duplex"),
            shutdown: Some(shutdown),
            worker: Some(worker),
        }
    }

    async fn h2<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(Request<h2::RecvStream>, SendResponse<Bytes>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::raw(move |socket| async move {
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (request, response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(request.version(), Version::HTTP_2);
            assert_eq!(request.method(), http::Method::POST);
            assert_eq!(request.uri().path(), "/synthetic-duplex");
            let exchange = serve(request, response);
            tokio::pin!(exchange);
            // h2 handles need their connection polled while either direction
            // waits. No detached connection driver or per-request worker.
            tokio::select! {
                _ = &mut exchange => {},
                result = poll_fn(|cx| connection.poll_closed(cx)) => {
                    panic!("HTTP/2 connection ended before the scripted exchange: {result:?}");
                },
            }
            // Flush queued DATA/trailers and keep driving until explicit stop.
            let _ = poll_fn(|cx| connection.poll_closed(cx)).await;
        })
        .await
    }

    async fn finish(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        within(self.worker.as_mut().unwrap())
            .await
            .expect("loopback server task panicked");
        self.worker.take();
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

struct ObservedUpload {
    receiver: ReceiverStream<UploadItem>,
    dropped: Option<oneshot::Sender<()>>,
}

impl Stream for ObservedUpload {
    type Item = UploadItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}

impl Drop for ObservedUpload {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

fn upload() -> (
    mpsc::Sender<UploadItem>,
    UploadStream,
    oneshot::Receiver<()>,
) {
    let (sender, receiver) = mpsc::channel(UPLOAD_CAPACITY);
    let (dropped, observed) = oneshot::channel();
    let stream = ObservedUpload {
        receiver: ReceiverStream::new(receiver),
        dropped: Some(dropped),
    };
    (sender, Box::pin(stream), observed)
}

fn client() -> ProxyClient {
    ProxyClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .build()
            .unwrap(),
    )
    .with_timeouts(UpstreamTimeouts {
        connect: WAIT,
        response_headers: WAIT,
        first_byte: WAIT,
        stream_idle: WAIT,
    })
}

async fn receive_upload(body: &mut h2::RecvStream, expected: &[u8]) {
    let mut bytes = BytesMut::new();
    while bytes.len() < expected.len() {
        let data = body
            .data()
            .await
            .expect("upload ended prematurely")
            .unwrap();
        body.flow_control().release_capacity(data.len()).unwrap();
        bytes.extend_from_slice(&data);
    }
    assert_eq!(&bytes[..], expected);
}

async fn expect_upload_eof(body: &mut h2::RecvStream) {
    // Hyper can encode the channel's EOF as a final zero-length DATA frame.
    while let Some(data) = body.data().await {
        assert!(
            data.unwrap().is_empty(),
            "unexpected upload after final frame"
        );
    }
    assert!(body.trailers().await.unwrap().is_none());
}

async fn expect_data(response: &mut ObservedDuplexResponse, expected: &[u8]) {
    let mut bytes = BytesMut::new();
    while bytes.len() < expected.len() {
        match within(response.frames.next()).await.unwrap().unwrap() {
            UpstreamFrame::Data(data) => bytes.extend_from_slice(&data),
            UpstreamFrame::Trailers(_) => panic!("trailers arrived before expected DATA"),
        }
    }
    assert_eq!(&bytes[..], expected);
}

fn request_error(result: anyhow::Result<ObservedDuplexResponse>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("request unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn wire_h2_response_precedes_upload_eof_then_accepts_second_frame_and_trailers() {
    let (complete, completed) = oneshot::channel();
    let server = LoopbackServer::h2(move |request, mut respond| async move {
        assert!(request.headers().get(http::header::CONTENT_TYPE).is_none());
        let mut upload = request.into_body();
        receive_upload(&mut upload, INITIAL).await;
        assert!(
            !upload.is_end_stream(),
            "headers must not wait for upload EOF"
        );
        let mut response = respond
            .send_response(
                Response::builder()
                    .status(201)
                    .header("x-synthetic-response", "present")
                    .body(())
                    .unwrap(),
                false,
            )
            .unwrap();
        response
            .send_data(Bytes::from_static(CHALLENGE), false)
            .unwrap();
        // The client cannot produce FOLLOW_UP until it receives CHALLENGE.
        receive_upload(&mut upload, FOLLOW_UP).await;
        assert!(
            !upload.is_end_stream(),
            "second upload frame must keep upload open"
        );
        response
            .send_data(Bytes::from_static(COMPLETE), false)
            .unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("0"));
        trailers.append("x-synthetic-trailer", HeaderValue::from_static("one"));
        trailers.append("x-synthetic-trailer", HeaderValue::from_static("two"));
        response.send_trailers(trailers).unwrap();
        // call_duplex's owner closes its sender after terminal response EOF.
        expect_upload_eof(&mut upload).await;
        complete.send(()).unwrap();
    })
    .await;
    let (sender, upload, dropped) = upload();
    assert_eq!(sender.max_capacity(), UPLOAD_CAPACITY);
    sender.try_send(Ok(Bytes::from_static(INITIAL))).unwrap();
    assert!(matches!(
        sender.try_send(Ok(Bytes::from_static(b"not-enqueued"))),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    let client = client();
    let mut response = within(client.call_duplex(
        &server.url,
        HeaderMap::new(),
        upload,
        CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.headers["x-synthetic-response"], "present");
    assert!(!sender.is_closed());
    expect_data(&mut response, CHALLENGE).await;
    within(sender.send(Ok(Bytes::from_static(FOLLOW_UP))))
        .await
        .unwrap();
    expect_data(&mut response, COMPLETE).await;
    match within(response.frames.next()).await.unwrap().unwrap() {
        UpstreamFrame::Trailers(trailers) => {
            assert_eq!(trailers["grpc-status"], "0");
            let values: Vec<_> = trailers
                .get_all("x-synthetic-trailer")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect();
            assert_eq!(values, ["one", "two"]);
        }
        UpstreamFrame::Data(_) => panic!("HTTP trailers were lost or converted to DATA"),
    }
    assert!(within(response.frames.next()).await.is_none());
    drop(sender);
    within(dropped)
        .await
        .expect("reqwest retained the upload worker after EOF");
    within(completed).await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn wire_h2_preserves_explicit_binary_content_type_and_headers_only_eof() {
    let (complete, completed) = oneshot::channel();
    let server = LoopbackServer::h2(move |request, mut respond| async move {
        assert_eq!(
            request.headers()[http::header::CONTENT_TYPE],
            "application/proto"
        );
        let mut upload = request.into_body();
        receive_upload(&mut upload, INITIAL).await;
        expect_upload_eof(&mut upload).await;
        respond
            .send_response(Response::builder().status(204).body(()).unwrap(), true)
            .unwrap();
        complete.send(()).unwrap();
    })
    .await;
    let (sender, upload, dropped) = upload();
    sender.send(Ok(Bytes::from_static(INITIAL))).await.unwrap();
    drop(sender);
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    let client = client();
    let mut response =
        within(client.call_duplex(&server.url, headers, upload, CancellationToken::new()))
            .await
            .unwrap();
    assert_eq!(response.status, 204);
    assert!(within(response.frames.next()).await.is_none());
    within(dropped).await.unwrap();
    within(completed).await.unwrap();
    server.finish().await;
}

#[derive(Clone, Copy)]
enum Termination {
    Cancel,
    Deadline,
    FirstByte,
    Idle,
    Drop,
}

async fn response_termination_releases_wire_stream(termination: Termination) {
    let (reset, reset_seen) = oneshot::channel();
    let server = LoopbackServer::h2(move |request, mut respond| async move {
        let mut upload = request.into_body();
        receive_upload(&mut upload, INITIAL).await;
        let mut response = respond.send_response(Response::new(()), false).unwrap();
        if matches!(termination, Termination::Idle) {
            response
                .send_data(Bytes::from_static(CHALLENGE), false)
                .unwrap();
        }
        // No response EOF and no server-side reset: release must come from the
        // client, not from this fixture or its shutdown path.
        let reason = poll_fn(|cx| response.poll_reset(cx)).await.unwrap();
        assert_eq!(reason, h2::Reason::CANCEL);
        reset.send(()).unwrap();
        drop(upload);
    })
    .await;
    let mut client = client();
    if matches!(termination, Termination::Deadline) {
        client = client.with_deadline(Deadline::from_now(Duration::from_secs(1)));
    }
    if matches!(termination, Termination::FirstByte | Termination::Idle) {
        client = client.with_timeouts(UpstreamTimeouts {
            first_byte: Duration::from_millis(100),
            stream_idle: Duration::from_millis(100),
            ..UpstreamTimeouts::default()
        });
    }
    let (sender, upload, dropped) = upload();
    sender.send(Ok(Bytes::from_static(INITIAL))).await.unwrap();
    let cancellation = CancellationToken::new();
    let mut response = Some(
        within(client.call_duplex(&server.url, HeaderMap::new(), upload, cancellation.clone()))
            .await
            .unwrap(),
    );
    if matches!(termination, Termination::Idle) {
        expect_data(response.as_mut().unwrap(), CHALLENGE).await;
    }
    if matches!(termination, Termination::Drop) {
        drop(response.take());
    } else {
        if matches!(termination, Termination::Cancel) {
            cancellation.cancel();
        }
        let error = within(response.as_mut().unwrap().frames.next())
            .await
            .expect("terminal error must be observable, not silently EOF")
            .unwrap_err();
        let expected = match termination {
            Termination::Cancel => "cancelled",
            Termination::Deadline => "Total",
            Termination::FirstByte => "FirstByte",
            Termination::Idle => "StreamIdle",
            Termination::Drop => unreachable!(),
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
    // The transport API requires its owner to close the upload on all terminal
    // paths. Keep the response stream alive after Err; it must already release
    // its body without needing an extra next() or a downstream drop.
    drop(sender);
    within(dropped)
        .await
        .expect("reqwest upload worker remained alive");
    within(reset_seen)
        .await
        .expect("server did not observe client RST_STREAM");
    if let Some(mut response) = response {
        assert!(within(response.frames.next()).await.is_none());
    }
    server.finish().await;
}

#[tokio::test]
async fn wire_h2_cancel_releases_body_before_next_poll() {
    response_termination_releases_wire_stream(Termination::Cancel).await;
}

#[tokio::test]
async fn wire_h2_total_deadline_releases_body_before_next_poll() {
    response_termination_releases_wire_stream(Termination::Deadline).await;
}

#[tokio::test]
async fn wire_h2_first_byte_timeout_releases_body_before_next_poll() {
    response_termination_releases_wire_stream(Termination::FirstByte).await;
}

#[tokio::test]
async fn wire_h2_idle_timeout_releases_body_before_next_poll() {
    response_termination_releases_wire_stream(Termination::Idle).await;
}

#[tokio::test]
async fn wire_h2_dropped_response_releases_upload_worker_and_resets_stream() {
    response_termination_releases_wire_stream(Termination::Drop).await;
}

async fn pending_headers_termination_releases_wire_stream(termination: Termination) {
    let (started, request_seen) = oneshot::channel();
    let (reset, reset_seen) = oneshot::channel();
    let server = LoopbackServer::h2(move |request, mut respond| async move {
        let mut upload = request.into_body();
        receive_upload(&mut upload, INITIAL).await;
        started.send(()).unwrap();
        let reason = poll_fn(|cx| respond.poll_reset(cx)).await.unwrap();
        assert_eq!(reason, h2::Reason::CANCEL);
        reset.send(()).unwrap();
        drop(upload);
    })
    .await;
    let mut client = client();
    if matches!(termination, Termination::Deadline) {
        client = client.with_deadline(Deadline::from_now(Duration::from_secs(1)));
    }
    let (sender, upload, dropped) = upload();
    sender.send(Ok(Bytes::from_static(INITIAL))).await.unwrap();
    let cancellation = CancellationToken::new();
    let mut call =
        Box::pin(client.call_duplex(&server.url, HeaderMap::new(), upload, cancellation.clone()));
    within(async {
        tokio::select! {
            result = &mut call => panic!("request ended before server observed upload: {:?}", result.err()),
            result = request_seen => result.unwrap(),
        }
    })
    .await;
    if !matches!(termination, Termination::Drop) {
        if matches!(termination, Termination::Cancel) {
            cancellation.cancel();
        }
        let error = request_error(within(&mut call).await);
        let expected = if matches!(termination, Termination::Cancel) {
            "cancelled"
        } else {
            "timeout"
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
    drop(call);
    drop(sender);
    within(dropped)
        .await
        .expect("pending-header upload worker remained alive");
    within(reset_seen)
        .await
        .expect("pending-header request was not reset");
    server.finish().await;
}

#[tokio::test]
async fn wire_h2_cancel_before_response_headers_releases_request() {
    pending_headers_termination_releases_wire_stream(Termination::Cancel).await;
}

#[tokio::test]
async fn wire_h2_deadline_before_response_headers_releases_request() {
    pending_headers_termination_releases_wire_stream(Termination::Deadline).await;
}

#[tokio::test]
async fn wire_h2_dropped_call_before_response_headers_releases_request() {
    pending_headers_termination_releases_wire_stream(Termination::Drop).await;
}

#[tokio::test]
async fn wire_cleartext_version_two_alone_does_not_enable_h2c_or_fall_back_to_http1() {
    let (observed, observation) = oneshot::channel();
    let server = LoopbackServer::raw(move |mut socket| async move {
        let mut first_bytes = [0; 24];
        let read =
            tokio::time::timeout(Duration::from_millis(200), socket.read(&mut first_bytes)).await;
        match read {
            Ok(Ok(0)) | Err(_) => {} // closed or idle, but no HTTP/1 request
            other => panic!(
                "version(HTTP_2) unexpectedly emitted cleartext bytes: {other:?}, {first_bytes:?}"
            ),
        }
        observed.send(()).unwrap();
    })
    .await;
    let ordinary_client = ProxyClient::new(reqwest::Client::builder().no_proxy().build().unwrap());
    let (sender, upload, dropped) = upload();
    sender.send(Ok(Bytes::from_static(INITIAL))).await.unwrap();
    let error = request_error(
        within(ordinary_client.call_duplex(
            &server.url,
            HeaderMap::new(),
            upload,
            CancellationToken::new(),
        ))
        .await,
    );
    assert!(
        error.to_string().contains("HTTP/2 request failed"),
        "{error:#}"
    );
    drop(sender);
    within(dropped).await.unwrap();
    within(observation).await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn wire_h2_required_rejects_http1_response_instead_of_accepting_fallback() {
    let (observed, observation) = oneshot::channel();
    let server = LoopbackServer::raw(move |mut socket| async move {
        let mut preface = [0; 24];
        socket.read_exact(&mut preface).await.unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
        observed.send(()).unwrap();
    })
    .await;
    let (sender, upload, dropped) = upload();
    sender.send(Ok(Bytes::from_static(INITIAL))).await.unwrap();
    let error = request_error(
        within(client().call_duplex(
            &server.url,
            HeaderMap::new(),
            upload,
            CancellationToken::new(),
        ))
        .await,
    );
    // reqwest/h2 can reject the HTTP/1 bytes before the defensive response
    // version guard is reached; either way call_duplex must never succeed.
    let message = error.to_string();
    assert!(
        message.contains("HTTP/2 request failed") || message.contains("required HTTP/2"),
        "{error:#}"
    );
    drop(sender);
    within(dropped).await.unwrap();
    within(observation).await.unwrap();
    server.finish().await;
}
