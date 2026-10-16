//! AWS event-stream framing used by Kiro's `generateAssistantResponse`.
//!
//! Frame layout (all integers big-endian):
//! `[total_len:4][headers_len:4][prelude_crc:4][headers][payload][message_crc:4]`.
//! Both CRCs are CRC-32/IEEE; the prelude CRC covers the first 8 bytes and the
//! message CRC covers everything before it. Frames and buffered input are
//! bounded so a hostile or broken upstream cannot grow memory without limit.
use std::collections::HashMap;

use crate::error::GatewayError;

/// Largest accepted frame (prelude + headers + payload + CRC).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
const PRELUDE: usize = 12;
const TRAILER: usize = 4;
/// Smallest legal frame: prelude plus the message CRC with no headers/payload.
const MIN_FRAME: usize = PRELUDE + TRAILER;

pub(super) fn protocol_error() -> GatewayError {
    GatewayError::StreamParseError {
        provider: "kiro".into(),
        raw_chunk: None,
    }
}

/// One decoded event-stream message. Only string headers are retained; every
/// other header type is skipped because Kiro routes purely on string headers.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub headers: HashMap<String, String>,
    pub payload: Vec<u8>,
}

impl Frame {
    /// `:message-type` header (`event`, `exception` or `error`).
    pub fn message_type(&self) -> &str {
        self.header(":message-type")
    }
    /// `:event-type` header, e.g. `assistantResponseEvent`.
    pub fn event_type(&self) -> &str {
        self.header(":event-type")
    }
    /// `:exception-type` header, e.g. `throttlingException`.
    pub fn exception_type(&self) -> &str {
        self.header(":exception-type")
    }
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map(String::as_str).unwrap_or("")
    }
}

/// Incremental decoder: feed arbitrary HTTP chunks, receive whole frames.
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    failed: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<Frame>, GatewayError> {
        if self.failed {
            return Err(protocol_error());
        }
        self.pending.extend_from_slice(input);
        let mut frames = Vec::new();
        let mut offset = 0;
        let outcome = loop {
            let rest = &self.pending[offset..];
            if rest.len() < PRELUDE {
                break Ok(());
            }
            let total = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as usize;
            let headers_len = u32::from_be_bytes(rest[4..8].try_into().unwrap()) as usize;
            let prelude_crc = u32::from_be_bytes(rest[8..12].try_into().unwrap());
            if crc32(&rest[..8]) != prelude_crc
                || !(MIN_FRAME..=MAX_FRAME).contains(&total)
                || headers_len > total - MIN_FRAME
            {
                break Err(protocol_error());
            }
            if rest.len() < total {
                break Ok(());
            }
            let frame = &rest[..total];
            let message_crc = u32::from_be_bytes(frame[total - TRAILER..].try_into().unwrap());
            if crc32(&frame[..total - TRAILER]) != message_crc {
                break Err(protocol_error());
            }
            let headers = match parse_headers(&frame[PRELUDE..PRELUDE + headers_len]) {
                Ok(headers) => headers,
                Err(error) => break Err(error),
            };
            frames.push(Frame {
                headers,
                payload: frame[PRELUDE + headers_len..total - TRAILER].to_vec(),
            });
            offset += total;
        };
        self.pending.drain(..offset);
        match outcome {
            Ok(()) => Ok(frames),
            Err(error) => {
                self.failed = true;
                self.pending.clear();
                Err(error)
            }
        }
    }

    /// Call when the HTTP body ends. A partial trailing frame is an error.
    pub fn finish(&self) -> Result<(), GatewayError> {
        if self.failed || !self.pending.is_empty() {
            return Err(protocol_error());
        }
        Ok(())
    }
}

fn parse_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>, GatewayError> {
    let mut headers = HashMap::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or_else(protocol_error)? as usize;
        let name = take(&mut bytes, 1, name_len)?;
        let name = std::str::from_utf8(name).map_err(|_| protocol_error())?;
        let value_type = *bytes.first().ok_or_else(protocol_error)?;
        bytes = &bytes[1..];
        match value_type {
            // Boolean true / false carry no value bytes.
            0 | 1 => {}
            // byte, int16, int32, int64, timestamp, uuid.
            2 => skip(&mut bytes, 1)?,
            3 => skip(&mut bytes, 2)?,
            4 => skip(&mut bytes, 4)?,
            5 | 8 => skip(&mut bytes, 8)?,
            9 => skip(&mut bytes, 16)?,
            // byte array / string: u16 length prefix.
            6 | 7 => {
                let len = u16::from_be_bytes(
                    take(&mut bytes, 0, 2)?
                        .try_into()
                        .map_err(|_| protocol_error())?,
                ) as usize;
                let value = take(&mut bytes, 0, len)?;
                if value_type == 7 {
                    let value = std::str::from_utf8(value).map_err(|_| protocol_error())?;
                    headers.insert(name.to_string(), value.to_string());
                }
            }
            _ => return Err(protocol_error()),
        }
    }
    Ok(headers)
}

/// Return `len` bytes after skipping `skip_prefix` bytes, advancing `bytes`.
fn take<'a>(
    bytes: &mut &'a [u8],
    skip_prefix: usize,
    len: usize,
) -> Result<&'a [u8], GatewayError> {
    let end = skip_prefix.checked_add(len).ok_or_else(protocol_error)?;
    if bytes.len() < end {
        return Err(protocol_error());
    }
    let value = &bytes[skip_prefix..end];
    *bytes = &bytes[end..];
    Ok(value)
}

fn skip(bytes: &mut &[u8], len: usize) -> Result<(), GatewayError> {
    take(bytes, 0, len).map(|_| ())
}

/// CRC-32/IEEE (polynomial 0xEDB88320), table generated at compile time.
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Encode one frame with string headers. Test and mock-server helper only.
#[cfg(test)]
pub fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7);
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total = PRELUDE + header_bytes.len() + payload.len() + TRAILER;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&frame[..8]);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    let message_crc = crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

/// Encode an `event` message of `event_type` carrying a JSON payload.
#[cfg(test)]
pub fn encode_event(event_type: &str, json: &str) -> Vec<u8> {
    encode_frame(
        &[
            (":message-type", "event"),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ],
        json.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_ieee_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn decodes_frames_split_at_every_byte_boundary() {
        let mut wire = encode_event("assistantResponseEvent", r#"{"content":"héllo"}"#);
        wire.extend(encode_event(
            "contextUsageEvent",
            r#"{"contextUsagePercentage":1.5}"#,
        ));
        let mut decoder = Decoder::new();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.feed(std::slice::from_ref(byte)).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event_type(), "assistantResponseEvent");
        assert_eq!(frames[0].message_type(), "event");
        assert_eq!(frames[0].payload, r#"{"content":"héllo"}"#.as_bytes());
        assert_eq!(frames[1].event_type(), "contextUsageEvent");
    }

    #[test]
    fn skips_non_string_headers_and_keeps_string_ones() {
        let mut headers = Vec::new();
        // bool true, int32, then a string header that must survive.
        headers.extend([1u8, b'a', 0]);
        headers.extend([1u8, b'b', 4, 0, 0, 0, 9]);
        headers.extend([11u8]);
        headers.extend(b":event-type");
        headers.extend([7u8, 0, 3]);
        headers.extend(b"foo");
        let parsed = parse_headers(&headers).unwrap();
        assert_eq!(parsed.get(":event-type").map(String::as_str), Some("foo"));
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn rejects_corrupt_crc_oversize_and_truncated_input() {
        let good = encode_event("assistantResponseEvent", r#"{"content":"x"}"#);

        let mut bad_prelude = good.clone();
        bad_prelude[8] ^= 0xFF;
        assert!(Decoder::new().feed(&bad_prelude).is_err());

        let mut bad_message = good.clone();
        let last = bad_message.len() - 1;
        bad_message[last] ^= 0xFF;
        assert!(Decoder::new().feed(&bad_message).is_err());

        let mut oversize = Vec::new();
        oversize.extend(((MAX_FRAME + 1) as u32).to_be_bytes());
        oversize.extend(0u32.to_be_bytes());
        let crc = crc32(&oversize);
        oversize.extend(crc.to_be_bytes());
        assert!(Decoder::new().feed(&oversize).is_err());

        let mut partial = Decoder::new();
        assert!(partial.feed(&good[..good.len() - 1]).unwrap().is_empty());
        assert!(partial.finish().is_err());
    }

    #[test]
    fn a_failed_decoder_stays_failed() {
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&[0xFF; 16]).is_err());
        assert!(decoder.feed(&[]).is_err());
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn headers_longer_than_the_frame_are_rejected() {
        // total = 16 (empty body) but headers_len claims 5 bytes.
        let mut frame = Vec::new();
        frame.extend(16u32.to_be_bytes());
        frame.extend(5u32.to_be_bytes());
        let crc = crc32(&frame);
        frame.extend(crc.to_be_bytes());
        frame.extend(0u32.to_be_bytes());
        assert!(Decoder::new().feed(&frame).is_err());
    }
}
