//! Event-boundary framing for byte-level SSE passthrough.
//!
//! Upstream HTTP chunks do not respect SSE event boundaries: a gateway may
//! flush `…"if"}}]}\n\ndata: {"id":"2026` and then stall. Forwarding such a
//! chunk verbatim leaves the client parser inside an unfinished event, so a
//! terminal error event the relay appends later (idle timeout, read error)
//! is concatenated onto that partial `data:` line and the client receives one
//! fused, unparseable event instead of a typed failure.
//!
//! [`SseFramer`] releases bytes only up to the last position where a
//! spec-compliant client parser holds no pending event, and withholds the
//! unfinished tail. Released bytes are the original bytes, never re-encoded.
//! Clients only dispatch an event at its terminating blank line, so holding
//! the tail adds no observable latency. LF, CRLF and bare CR terminators are
//! supported, split across chunks anywhere.

use bytes::{Bytes, BytesMut};

/// Upper bound on a withheld unfinished event. Past it the framer degrades to
/// verbatim forwarding (the pre-framing behavior) instead of buffering
/// without limit; a single event this large is not a real model stream.
const MAX_UNFINISHED_EVENT_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct SseFramer {
    /// Bytes scanned but not yet released, starting at the last safe cut.
    pending: BytesMut,
    /// First byte of the line currently being scanned (`None` before it).
    line_first: Option<u8>,
    /// A field line was seen since the last dispatch: an event is pending.
    in_event: bool,
    /// The previous byte was CR; CRLF vs. bare CR depends on the next byte.
    cr_pending: bool,
    limit: usize,
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::with_limit(MAX_UNFINISHED_EVENT_BYTES)
    }
}

impl SseFramer {
    fn with_limit(limit: usize) -> Self {
        Self {
            pending: BytesMut::new(),
            line_first: None,
            in_event: false,
            cr_pending: false,
            limit,
        }
    }

    /// Feed one upstream chunk; returns the bytes that are now safe to
    /// forward (complete events, comments and blank lines), if any.
    pub(super) fn push(&mut self, chunk: Bytes) -> Option<Bytes> {
        let base = self.pending.len();
        let cut = self.scan(&chunk, base);
        if base == 0 && cut == Some(chunk.len()) {
            return Some(chunk); // chunk ends on a boundary: no copy
        }
        self.pending.extend_from_slice(&chunk);
        let cut = match cut {
            Some(cut) => cut,
            None if self.pending.len() > self.limit => self.pending.len(),
            None => return None,
        };
        let released = self.pending.split_to(cut).freeze();
        if self.pending.len() > self.limit {
            // Degraded: keep the parse state, give up withholding this event.
            let mut out = BytesMut::from(&released[..]);
            out.extend_from_slice(&self.pending.split());
            return Some(out.freeze());
        }
        (!released.is_empty()).then_some(released)
    }

    /// Upstream EOF: release the withheld tail verbatim, exactly as received.
    pub(super) fn finish(&mut self) -> Option<Bytes> {
        (!self.pending.is_empty()).then(|| self.pending.split().freeze())
    }

    /// Terminal failure: drop the withheld unfinished event so the caller's
    /// error event starts on a clean boundary. Returns the dropped byte count.
    pub(super) fn discard(&mut self) -> usize {
        let dropped = self.pending.len();
        self.pending.clear();
        dropped
    }

    /// Advance the parse state over `bytes` (which begin at absolute offset
    /// `base` within `pending`); returns the last safe cut, if one was found.
    fn scan(&mut self, bytes: &[u8], base: usize) -> Option<usize> {
        let mut cut = None;
        for (offset, &byte) in bytes.iter().enumerate() {
            let at = base + offset;
            if std::mem::take(&mut self.cr_pending) {
                if byte == b'\n' {
                    self.end_line(at + 1, &mut cut); // CRLF
                    continue;
                }
                self.end_line(at, &mut cut); // bare CR; `byte` starts a line
            }
            match byte {
                b'\r' => self.cr_pending = true,
                b'\n' => self.end_line(at + 1, &mut cut),
                _ => {
                    self.line_first.get_or_insert(byte);
                }
            }
        }
        cut
    }

    /// A line ended just before absolute offset `end`.
    fn end_line(&mut self, end: usize, cut: &mut Option<usize>) {
        match self.line_first.take() {
            // Blank line: dispatches any pending event.
            None => {
                self.in_event = false;
                *cut = Some(end);
            }
            // Comment outside an event (keep-alive): nothing is pending.
            Some(b':') if !self.in_event => *cut = Some(end),
            Some(b':') => {}
            Some(_) => self.in_event = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(framer: &mut SseFramer, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in chunks {
            if let Some(bytes) = framer.push(Bytes::copy_from_slice(chunk)) {
                out.extend_from_slice(&bytes);
            }
        }
        out
    }

    #[test]
    fn complete_chunks_are_forwarded_without_copy() {
        let mut framer = SseFramer::default();
        let chunk = Bytes::from_static(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        let out = framer.push(chunk.clone()).expect("complete events");
        assert_eq!(
            out.as_ptr(),
            chunk.as_ptr(),
            "boundary-aligned chunk is not copied"
        );
        assert!(framer.finish().is_none());
    }

    #[test]
    fn unfinished_tail_is_withheld_until_its_boundary() {
        // Shape of the production stall: one event, then a cut inside JSON.
        let mut framer = SseFramer::default();
        let out = feed(
            &mut framer,
            &[b"data: {\"x\":\"if\"}\n\ndata: {\"id\":\"2026"],
        );
        assert_eq!(out, b"data: {\"x\":\"if\"}\n\n");
        let out = feed(&mut framer, &[b"1022\"}\n\n"]);
        assert_eq!(out, b"data: {\"id\":\"20261022\"}\n\n");
    }

    #[test]
    fn discard_drops_the_unfinished_event_for_a_clean_error_boundary() {
        let mut framer = SseFramer::default();
        let out = feed(&mut framer, &[b"data: {\"x\":1}\n\ndata: {\"id\":\"2026"]);
        assert_eq!(out, b"data: {\"x\":1}\n\n");
        assert_eq!(framer.discard(), b"data: {\"id\":\"2026".len());
        assert!(
            framer.finish().is_none(),
            "discarded bytes must never be flushed"
        );
    }

    #[test]
    fn eof_flushes_the_tail_verbatim() {
        let mut framer = SseFramer::default();
        let out = feed(&mut framer, &[b"data: {\"x\":1}\n\ndata: [DONE]"]);
        assert_eq!(out, b"data: {\"x\":1}\n\n");
        assert_eq!(framer.finish().as_deref(), Some(&b"data: [DONE]"[..]));
    }

    #[test]
    fn keepalive_comments_pass_outside_events_but_not_inside() {
        let mut framer = SseFramer::default();
        assert_eq!(feed(&mut framer, &[b": ping\n"]), b": ping\n");
        // A comment inside an event does not end it: still nothing to release.
        assert!(feed(&mut framer, &[b"event: delta\n: ping\n"]).is_empty());
        assert_eq!(
            feed(&mut framer, &[b"data: {}\n\n"]),
            b"event: delta\n: ping\ndata: {}\n\n"
        );
    }

    #[test]
    fn crlf_split_between_cr_and_lf_is_one_terminator() {
        let mut framer = SseFramer::default();
        assert!(feed(&mut framer, &[b"data: {}\r\n\r"]).is_empty());
        assert_eq!(feed(&mut framer, &[b"\n"]), b"data: {}\r\n\r\n");
        assert!(framer.finish().is_none());
    }

    #[test]
    fn bare_cr_terminators_are_resolved_by_the_next_byte() {
        let mut framer = SseFramer::default();
        assert!(feed(&mut framer, &[b"data: x\r\r"]).is_empty());
        assert_eq!(feed(&mut framer, &[b"data: y\r\r"]), b"data: x\r\r");
        assert_eq!(framer.finish().as_deref(), Some(&b"data: y\r\r"[..]));
    }

    #[test]
    fn oversized_unfinished_event_degrades_to_verbatim_forwarding() {
        let mut framer = SseFramer::with_limit(8);
        assert_eq!(
            feed(&mut framer, &[b"data: 0123456789"]),
            b"data: 0123456789"
        );
        // Parse state survives the degrade: the event still ends at its blank line.
        assert_eq!(feed(&mut framer, &[b"\n\n"]), b"\n\n");
        assert_eq!(
            feed(&mut framer, &[b"data: {}\n\ndata: {"]),
            b"data: {}\n\n"
        );
    }

    /// Every chunking of a mixed-terminator wire must reproduce it byte for
    /// byte and release only at safe offsets, as early as the bytes allow.
    #[test]
    fn arbitrary_chunking_is_byte_exact_and_releases_only_at_safe_offsets() {
        let segments: [&[u8]; 6] = [
            b"data: a\n\n",                            // LF event
            b"data: b\r\r",                            // bare-CR event
            b": ping\n",                               // keep-alive outside an event
            b"data: c1\r\ndata: c2\r\n\r\n",           // CRLF multi-line event
            b"event: e\n: inner\ndata: {\"d\":1}\n\n", // comment inside an event
            b"\n",                                     // stray blank line
        ];
        let wire: Vec<u8> = segments.concat();
        let mut safe = vec![0];
        for segment in segments {
            safe.push(safe.last().unwrap() + segment.len());
        }
        // A bare CR at the end of the bytes seen so far is not yet resolved.
        let releasable = |seen: usize| {
            *safe
                .iter()
                .filter(|&&offset| offset <= seen)
                .filter(|&&offset| !(offset == seen && offset > 0 && wire[offset - 1] == b'\r'))
                .max()
                .unwrap()
        };

        for first in 0..=wire.len() {
            for second in first..=wire.len() {
                let mut framer = SseFramer::default();
                let mut released = 0;
                for (start, end) in [(0, first), (first, second), (second, wire.len())] {
                    if let Some(bytes) = framer.push(Bytes::copy_from_slice(&wire[start..end])) {
                        assert_eq!(&bytes[..], &wire[released..released + bytes.len()]);
                        released += bytes.len();
                    }
                    assert_eq!(
                        released,
                        releasable(end),
                        "splits {first}/{second}, seen {end}"
                    );
                }
                if let Some(bytes) = framer.finish() {
                    assert_eq!(&bytes[..], &wire[released..]);
                    released += bytes.len();
                }
                assert_eq!(released, wire.len(), "splits {first}/{second}");
            }
        }
    }
}
