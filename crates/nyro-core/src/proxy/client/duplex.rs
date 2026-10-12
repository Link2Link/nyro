//! HTTP/2 duplex transport. No JSON, protobuf, or provider policy lives here.
//! Keep HTTP trailers as frames rather than dropping them through bytes_stream.
#[cfg(test)]
#[path = "duplex_tests.rs"]
mod tests;

use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http_body_util::BodyExt;
use reqwest::header::HeaderMap;
use tokio::time::Instant;

use super::{ProxyClient, UpstreamTimeoutPhase};
use crate::proxy::context::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DuplexError {
    #[error("upstream request cancelled")]
    Cancelled,
    #[error("upstream timeout during {0:?}")]
    Timeout(UpstreamTimeoutPhase),
}

pub(crate) type UploadStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;
pub(crate) type FrameStream = Pin<Box<dyn Stream<Item = Result<UpstreamFrame>> + Send>>;

#[derive(Debug)]
pub(crate) enum UpstreamFrame {
    Data(Bytes),
    Trailers(HeaderMap),
}

pub(crate) struct ObservedDuplexResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub frames: FrameStream,
}

impl ProxyClient {
    /// The caller owns the upload sender and closes it on every terminal path.
    /// A dropped response stream drops the reqwest body (and HTTP/2 stream).
    pub(crate) async fn call_duplex(
        &self,
        url: &str,
        headers: HeaderMap,
        upload: UploadStream,
        cancellation: CancellationToken,
    ) -> Result<ObservedDuplexResponse> {
        if cancellation.is_cancelled() {
            return Err(DuplexError::Cancelled.into());
        }
        if self.deadline.is_exceeded() {
            return Err(DuplexError::Timeout(UpstreamTimeoutPhase::Total).into());
        }
        if let Some(attempt) = &self.performance {
            attempt.request_headers(&headers);
        }
        let header_until = (Instant::now() + self.timeouts.response_headers)
            .min(Instant::from_std(self.deadline.at()));
        let request = self
            .http
            .post(url)
            .version(reqwest::Version::HTTP_2)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(upload));
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(DuplexError::Cancelled.into()),
            result = tokio::time::timeout_at(header_until, request.send()) => {
                result.map_err(|_| DuplexError::Timeout(if header_until == Instant::from_std(self.deadline.at()) { UpstreamTimeoutPhase::Total } else { UpstreamTimeoutPhase::ResponseHeaders }))?
                    .map_err(|error| error.without_url()).context("HTTP/2 request failed")?
            }
        };
        if response.version() != reqwest::Version::HTTP_2 {
            bail!("upstream did not negotiate required HTTP/2");
        }
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        if let Some(attempt) = &self.performance {
            attempt.response(status, &headers);
        }
        let response: http::Response<reqwest::Body> = response.into();
        let state = FrameState {
            body: Some(response.into_body()),
            cancellation,
            total_until: Instant::from_std(self.deadline.at()),
            first_until: Instant::now() + self.timeouts.first_byte,
            idle: self.timeouts.stream_idle,
            saw_data: false,
            finished: false,
            performance: self.performance.clone(),
        };
        let frames = futures::stream::unfold(state, |mut state| async move {
            if state.finished {
                return None;
            }
            let mut until = if state.saw_data {
                Instant::now() + state.idle
            } else {
                state.first_until
            };
            until = until.min(state.total_until);
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = state.cancellation.cancelled() => {
                        state.finished = true;
                        state.body.take();
                        return Some((Err(DuplexError::Cancelled.into()), state));
                    }
                    result = tokio::time::timeout_at(until, state.body.as_mut().expect("active stream has a body").frame()) => result,
                };
                match frame {
                    Err(_) => {
                        state.finished = true;
                        state.body.take();
                        let phase = if until == state.total_until {
                            UpstreamTimeoutPhase::Total
                        } else if state.saw_data {
                            UpstreamTimeoutPhase::StreamIdle
                        } else {
                            UpstreamTimeoutPhase::FirstByte
                        };
                        return Some((Err(DuplexError::Timeout(phase).into()), state));
                    }
                    Ok(None) => {
                        if let Some(attempt) = &state.performance {
                            attempt.eof();
                        }
                        return None;
                    }
                    Ok(Some(Err(error))) => {
                        state.finished = true;
                        state.body.take();
                        return Some((
                            Err(anyhow!(error.without_url()).context("HTTP/2 frame read failed")),
                            state,
                        ));
                    }
                    Ok(Some(Ok(frame))) => match frame.into_data() {
                        Ok(bytes) if bytes.is_empty() => continue,
                        Ok(bytes) => {
                            state.saw_data = true;
                            if let Some(attempt) = &state.performance {
                                attempt.chunk(&bytes);
                            }
                            return Some((Ok(UpstreamFrame::Data(bytes)), state));
                        }
                        Err(frame) => {
                            if let Ok(trailers) = frame.into_trailers() {
                                return Some((Ok(UpstreamFrame::Trailers(trailers)), state));
                            }
                        }
                    },
                }
            }
        });
        Ok(ObservedDuplexResponse {
            status,
            headers,
            frames: Box::pin(frames),
        })
    }

    /// Unary protobuf calls share identity, proxy, HTTP/2 and frame limits.
    pub(crate) async fn call_http2_unary(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Bytes,
        cancellation: CancellationToken,
        max_body: usize,
    ) -> Result<(u16, HeaderMap, Bytes)> {
        let upload = Box::pin(futures::stream::once(async move { Ok(body) }));
        let mut response = self.call_duplex(url, headers, upload, cancellation).await?;
        let mut collected = bytes::BytesMut::new();
        while let Some(frame) = response.frames.next().await {
            match frame? {
                UpstreamFrame::Data(bytes) => {
                    if collected.len().saturating_add(bytes.len()) > max_body {
                        bail!("upstream unary response exceeds size limit");
                    }
                    collected.extend_from_slice(&bytes);
                }
                UpstreamFrame::Trailers(trailers) => {
                    if let Some(status) = trailers.get("grpc-status") {
                        if status != "0" {
                            bail!("upstream unary RPC returned non-success trailers");
                        }
                    }
                }
            }
        }
        Ok((response.status, response.headers, collected.freeze()))
    }
}

struct FrameState {
    body: Option<reqwest::Body>,
    cancellation: CancellationToken,
    total_until: Instant,
    first_until: Instant,
    idle: Duration,
    saw_data: bool,
    finished: bool,
    performance: Option<crate::performance::Attempt>,
}
