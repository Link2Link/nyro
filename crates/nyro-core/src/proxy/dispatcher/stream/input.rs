//! Byte-to-text boundary for native upstream stream conversion.
//!
//! Transport chunks are not SSE lines or UTF-8 boundaries. Retain an unfinished
//! line before invoking vendor hooks (which may parse a whole JSON data line),
//! then let the protocol decoder assemble SSE events. The same state must live
//! across the pre-commit probe, forwarding loop, and EOF flush.

#[cfg(test)]
mod tests;

use crate::protocol::StreamResponseDecoder;
use crate::protocol::ir::AiStreamDelta;
use crate::proxy::client::MAX_UPSTREAM_RESPONSE_BODY_BYTES;

use super::StreamRawChunkHook;

#[derive(Default)]
pub(in crate::proxy::dispatcher) struct StreamInput {
    pending_line: Vec<u8>,
    previous_was_cr: bool,
}

impl StreamInput {
    /// Emit complete, UTF-8-validated lines with normalized SSE line endings.
    /// A CR ends a line immediately; its optional LF may arrive in another chunk.
    fn push(&mut self, bytes: &[u8]) -> anyhow::Result<String> {
        let mut ready = Vec::new();
        for &byte in bytes {
            if byte == b'\n' && self.previous_was_cr {
                self.previous_was_cr = false;
                continue;
            }
            self.previous_was_cr = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                ready.extend_from_slice(&self.pending_line);
                ready.push(b'\n');
                self.pending_line.clear();
            } else {
                anyhow::ensure!(
                    self.pending_line.len() < MAX_UPSTREAM_RESPONSE_BODY_BYTES,
                    "upstream stream line exceeds the processing size limit"
                );
                self.pending_line.push(byte);
            }
        }
        Ok(String::from_utf8(ready)?)
    }

    pub(in crate::proxy::dispatcher) async fn decode_chunk(
        &mut self,
        bytes: &[u8],
        parser: &mut dyn StreamResponseDecoder,
        hook: Option<&StreamRawChunkHook>,
        performance: Option<&crate::performance::Attempt>,
    ) -> anyhow::Result<Vec<AiStreamDelta>> {
        let text = self.push(bytes)?;
        Self::decode_text(text, parser, hook, performance).await
    }

    pub(in crate::proxy::dispatcher) async fn finish(
        &mut self,
        parser: &mut dyn StreamResponseDecoder,
        hook: Option<&StreamRawChunkHook>,
        performance: Option<&crate::performance::Attempt>,
    ) -> anyhow::Result<Vec<AiStreamDelta>> {
        // Do not repair invalid/truncated UTF-8 into apparently valid tool input.
        let tail = String::from_utf8(std::mem::take(&mut self.pending_line))?;
        let mut deltas = Self::decode_text(tail, parser, hook, performance).await?;
        deltas.extend(parser.finish()?);
        Ok(deltas)
    }

    async fn decode_text(
        text: String,
        parser: &mut dyn StreamResponseDecoder,
        hook: Option<&StreamRawChunkHook>,
        performance: Option<&crate::performance::Attempt>,
    ) -> anyhow::Result<Vec<AiStreamDelta>> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let text = if let Some(hook) = hook {
            hook.apply(&text, performance).await
        } else {
            text
        };
        parser.parse_chunk(&text)
    }
}
