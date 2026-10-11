//! Read-side inference timeouts, measured on original upstream bytes.
//!
//! Idle time starts when the consumer asks for the next chunk, not while it
//! is processing a previous one. Empty chunks do not reset a pending read.
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use tokio::time::{Instant, Sleep};

use crate::config::UpstreamTimeouts;
use crate::proxy::context::Deadline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamTimeoutPhase {
    ResponseHeaders,
    FirstByte,
    StreamIdle,
    Total,
}

impl UpstreamTimeoutPhase {
    pub fn stage(self) -> &'static str {
        match self {
            Self::ResponseHeaders => "upstream_headers",
            Self::FirstByte => "upstream_first_byte",
            Self::StreamIdle => "upstream_idle",
            Self::Total => "request_deadline",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamReadError {
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error("upstream timeout during {phase:?}")]
    Timeout { phase: UpstreamTimeoutPhase },
}

impl UpstreamReadError {
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Transport(error) => error.is_timeout(),
            Self::Timeout { .. } => true,
        }
    }

    pub fn stage(&self) -> &'static str {
        match self {
            Self::Timeout { phase } => phase.stage(),
            Self::Transport(error) if error.is_connect() => "upstream_connect",
            Self::Transport(_) => "upstream_read",
        }
    }
}

pub(super) fn bounded_timeout(
    phase_at: Instant,
    phase: UpstreamTimeoutPhase,
    deadline: &Deadline,
) -> (Instant, UpstreamTimeoutPhase) {
    let total_at = Instant::from_std(deadline.at());
    if total_at <= phase_at {
        (total_at, UpstreamTimeoutPhase::Total)
    } else {
        (phase_at, phase)
    }
}

pub(super) struct TimedReadStream<S> {
    inner: Option<Pin<Box<S>>>,
    timeouts: UpstreamTimeouts,
    deadline: Deadline,
    first_at: Instant,
    saw_bytes: bool,
    timer: Option<(Pin<Box<Sleep>>, UpstreamTimeoutPhase)>,
}

impl<S> TimedReadStream<S> {
    pub(super) fn new(
        inner: S,
        timeouts: UpstreamTimeouts,
        deadline: Deadline,
        headers_at: Instant,
    ) -> Self {
        Self {
            inner: Some(Box::pin(inner)),
            timeouts,
            deadline,
            first_at: headers_at + timeouts.first_byte,
            saw_bytes: false,
            timer: None,
        }
    }
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>>> Stream for TimedReadStream<S> {
    type Item = Result<Bytes, UpstreamReadError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if self.deadline.is_exceeded() {
            self.inner = None;
            self.timer = None;
            return Poll::Ready(Some(Err(UpstreamReadError::Timeout {
                phase: UpstreamTimeoutPhase::Total,
            })));
        }
        if self.timer.is_none() {
            let (at, phase) = if self.saw_bytes {
                (
                    Instant::now() + self.timeouts.stream_idle,
                    UpstreamTimeoutPhase::StreamIdle,
                )
            } else {
                (self.first_at, UpstreamTimeoutPhase::FirstByte)
            };
            let (at, phase) = bounded_timeout(at, phase, &self.deadline);
            self.timer = Some((Box::pin(tokio::time::sleep_until(at)), phase));
        }
        let (timer, phase) = self.timer.as_mut().unwrap();
        if timer.as_mut().poll(cx).is_ready() {
            let phase = *phase;
            self.inner = None;
            self.timer = None;
            return Poll::Ready(Some(Err(UpstreamReadError::Timeout { phase })));
        }
        // Bound work per poll even if an unusual transport emits endless empty
        // chunks; keep the same timer across every empty chunk / wakeup.
        for _ in 0..32 {
            match self.inner.as_mut().unwrap().as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) if bytes.is_empty() => continue,
                Poll::Ready(Some(Ok(bytes))) => {
                    self.saw_bytes = true;
                    self.timer = None;
                    return Poll::Ready(Some(Ok(bytes)));
                }
                Poll::Ready(Some(Err(error))) => {
                    self.inner = None;
                    self.timer = None;
                    return Poll::Ready(Some(Err(error.into())));
                }
                Poll::Ready(None) => {
                    self.inner = None;
                    self.timer = None;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt, StreamExt};
    use std::time::Duration;

    fn policy() -> UpstreamTimeouts {
        UpstreamTimeouts {
            first_byte: Duration::from_secs(3),
            stream_idle: Duration::from_secs(2),
            ..Default::default()
        }
    }

    fn assert_timeout(item: Option<Result<Bytes, UpstreamReadError>>, phase: UpstreamTimeoutPhase) {
        assert!(
            matches!(item, Some(Err(UpstreamReadError::Timeout { phase: actual })) if actual == phase)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn active_stream_can_run_beyond_old_total_timeout() {
        let started = Instant::now();
        let source = futures::stream::unfold(0, |n| async move {
            if n == 700 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            Some((Ok(Bytes::from_static(b"data: reasoning\n\n")), n + 1))
        });
        let mut stream = TimedReadStream::new(source, policy(), Deadline::never(), started);
        for _ in 0..700 {
            assert!(stream.next().await.unwrap().is_ok());
        }
        assert!(stream.next().await.is_none());
        assert!(started.elapsed() > Duration::from_secs(600));
    }

    #[tokio::test(start_paused = true)]
    async fn first_byte_budget_starts_at_response_headers() {
        let mut stream = TimedReadStream::new(
            futures::stream::pending(),
            policy(),
            Deadline::never(),
            Instant::now(),
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_timeout(stream.next().await, UpstreamTimeoutPhase::FirstByte);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_is_reset_by_nonempty_data_not_consumer_delay() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let source = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut stream = TimedReadStream::new(source, policy(), Deadline::never(), Instant::now());
        tx.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        // Downstream backpressure / hooks can take longer than the idle budget.
        tokio::time::advance(Duration::from_secs(800)).await;
        tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        let waiting_at = Instant::now();
        assert_timeout(stream.next().await, UpstreamTimeoutPhase::StreamIdle);
        assert_eq!(waiting_at.elapsed(), policy().stream_idle);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn empty_chunks_and_repeated_polls_do_not_extend_first_byte_budget() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let source = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut stream = TimedReadStream::new(source, policy(), Deadline::never(), Instant::now());
        assert!(stream.next().now_or_never().is_none());
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tx.send(Ok(Bytes::new())).await.unwrap();
            assert!(stream.next().now_or_never().is_none());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_timeout(stream.next().await, UpstreamTimeoutPhase::FirstByte);
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_total_deadline_still_wins_over_phase_budget() {
        let source = futures::stream::iter([Ok(Bytes::from_static(b"data"))]);
        let mut stream = TimedReadStream::new(
            source,
            policy(),
            Deadline::from_now(Duration::ZERO),
            Instant::now(),
        );
        assert_timeout(stream.next().await, UpstreamTimeoutPhase::Total);
    }
}
