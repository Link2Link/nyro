//! Last-mile wire hook for provider channels that must see the exact bytes.
//!
//! Some upstream contracts cannot be met before the body is serialized: the
//! Claude Code OAuth channel signs the final request bytes (`cch`) and
//! restores tool names in the response. `ProxyClient` is the one boundary
//! every request path (native passthrough, IR encode, raw-wire compat) and
//! every response consumer (streaming, buffered, error bodies) shares, so
//! the hook lives here.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::Stream;

use super::UpstreamReadError;

pub(crate) trait EgressHook: Send + Sync + 'static {
    /// Final request body, given the serialized bytes about to be sent.
    fn finalize_request_body(&self, body: Bytes) -> Bytes {
        body
    }

    /// Whether [`rewrite_response_line`]/[`rewrite_response_json`] can
    /// change anything; when `false` the response stream is not wrapped.
    fn rewrites_response(&self) -> bool {
        false
    }

    /// Rewrite one complete SSE line (no trailing newline). `None` keeps it.
    fn rewrite_response_line(&self, _line: &str) -> Option<String> {
        None
    }

    /// Rewrite a complete non-SSE (JSON) response body. `None` keeps it.
    fn rewrite_response_json(&self, _body: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No non-whitespace byte seen yet.
    Undecided,
    /// A JSON document: buffer to EOF, then rewrite once.
    Json,
    /// SSE: rewrite complete lines as they arrive, hold the partial tail.
    Sse,
}

/// Applies an [`EgressHook`]'s response rewrite to an upstream byte stream.
/// SSE output keeps flowing line by line; JSON bodies are buffered (they are
/// whole documents to every consumer anyway).
pub(crate) struct RewriteStream<S> {
    inner: S,
    hook: Arc<dyn EgressHook>,
    mode: Mode,
    pending: BytesMut,
    deferred_error: Option<UpstreamReadError>,
    done: bool,
}

impl<S> RewriteStream<S> {
    pub(crate) fn new(inner: S, hook: Arc<dyn EgressHook>) -> Self {
        Self {
            inner,
            hook,
            mode: Mode::Undecided,
            pending: BytesMut::new(),
            deferred_error: None,
            done: false,
        }
    }

    fn decide(&mut self) {
        if self.mode != Mode::Undecided {
            return;
        }
        if let Some(&first) = self.pending.iter().find(|b| !b.is_ascii_whitespace()) {
            self.mode = if matches!(first, b'{' | b'[') {
                Mode::Json
            } else {
                Mode::Sse
            };
        }
    }

    fn rewrite_lines(&self, chunk: &[u8]) -> Bytes {
        let mut out = BytesMut::with_capacity(chunk.len());
        let mut first = true;
        for line in chunk.split(|b| *b == b'\n') {
            if !first {
                out.extend_from_slice(b"\n");
            }
            first = false;
            let (content, cr) = match line.strip_suffix(b"\r") {
                Some(content) => (content, true),
                None => (line, false),
            };
            let rewritten = std::str::from_utf8(content)
                .ok()
                .and_then(|text| self.hook.rewrite_response_line(text));
            match rewritten {
                Some(text) => out.extend_from_slice(text.as_bytes()),
                None => out.extend_from_slice(content),
            }
            if cr {
                out.extend_from_slice(b"\r");
            }
        }
        out.freeze()
    }

    /// Emit every complete SSE line held in `pending`.
    fn take_complete_lines(&mut self) -> Option<Bytes> {
        let last_newline = self.pending.iter().rposition(|b| *b == b'\n')?;
        let complete = self.pending.split_to(last_newline + 1);
        Some(self.rewrite_lines(&complete))
    }

    /// Everything still held, rewritten for the current mode.
    fn flush(&mut self) -> Option<Bytes> {
        if self.pending.is_empty() {
            return None;
        }
        let held = self.pending.split().freeze();
        Some(match self.mode {
            Mode::Sse => self.rewrite_lines(&held),
            Mode::Json | Mode::Undecided => self
                .hook
                .rewrite_response_json(&held)
                .map(Bytes::from)
                .unwrap_or(held),
        })
    }
}

impl<S> Stream for RewriteStream<S>
where
    S: Stream<Item = Result<Bytes, UpstreamReadError>> + Unpin,
{
    type Item = Result<Bytes, UpstreamReadError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(error) = this.deferred_error.take() {
            this.done = true;
            return Poll::Ready(Some(Err(error)));
        }
        if this.done {
            return Poll::Ready(None);
        }
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => {
                    this.pending.extend_from_slice(&chunk);
                    this.decide();
                    if this.mode == Mode::Sse
                        && let Some(ready) = this.take_complete_lines()
                    {
                        return Poll::Ready(Some(Ok(ready)));
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    // Hand over whatever was held before surfacing the error.
                    return match this.flush() {
                        Some(held) => {
                            this.deferred_error = Some(error);
                            Poll::Ready(Some(Ok(held)))
                        }
                        None => {
                            this.done = true;
                            Poll::Ready(Some(Err(error)))
                        }
                    };
                }
                Poll::Ready(None) => {
                    this.done = true;
                    return Poll::Ready(this.flush().map(Ok));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    struct Upper;

    impl EgressHook for Upper {
        fn rewrites_response(&self) -> bool {
            true
        }
        fn rewrite_response_line(&self, line: &str) -> Option<String> {
            line.starts_with("data:").then(|| line.to_uppercase())
        }
        fn rewrite_response_json(&self, body: &[u8]) -> Option<Vec<u8>> {
            Some(body.to_ascii_uppercase())
        }
    }

    async fn collect(chunks: Vec<&'static str>) -> String {
        let inner = futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, UpstreamReadError>(Bytes::from_static(c.as_bytes()))),
        );
        let stream = RewriteStream::new(inner, Arc::new(Upper));
        let parts: Vec<Bytes> = stream.map(|r| r.unwrap()).collect().await;
        String::from_utf8(parts.concat()).unwrap()
    }

    #[tokio::test]
    async fn sse_lines_split_across_chunks_are_rewritten_whole() {
        let out = collect(vec!["event: x\nda", "ta: abc\r\n", "\ndata: tail"]).await;
        assert_eq!(out, "event: x\nDATA: ABC\r\n\nDATA: TAIL");
    }

    #[tokio::test]
    async fn json_bodies_are_buffered_and_rewritten_once() {
        let out = collect(vec!["  {\"a\":", "\"b\"}"]).await;
        assert_eq!(out, "  {\"A\":\"B\"}");
    }
}
