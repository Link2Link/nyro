//! Devin Connect framing and the observed Codeium protobuf subset.
//! Unknown protobuf fields are ignored; frames and inflated data are bounded.
use crate::error::GatewayError;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use prost::Message;
use std::io::Read;

pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MAX_INFLATED: usize = 64 * 1024 * 1024;
const TOOL_ID_PREFIX: &str = "cpa_tid_v1_";

pub(super) fn protocol_error() -> GatewayError {
    GatewayError::StreamParseError {
        provider: "devin".into(),
        raw_chunk: None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    End(Vec<u8>),
}

#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    expected: Option<usize>,
    ended: bool,
    failed: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, mut input: &[u8]) -> Result<Vec<Frame>, GatewayError> {
        if self.failed || (self.ended && !input.is_empty()) {
            return Err(protocol_error());
        }
        let result = (|| {
            let mut frames = Vec::new();
            while !input.is_empty() {
                if self.ended {
                    return Err(protocol_error());
                }
                let target = self.expected.unwrap_or(5);
                let count = (target - self.pending.len()).min(input.len());
                self.pending.extend_from_slice(&input[..count]);
                input = &input[count..];
                if self.pending.len() < target {
                    break;
                }
                if self.expected.is_none() {
                    let flag = self.pending[0];
                    if flag > 3 {
                        return Err(protocol_error());
                    }
                    let length =
                        u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
                    if length > MAX_FRAME {
                        return Err(protocol_error());
                    }
                    self.expected = Some(5 + length);
                    if length != 0 {
                        continue;
                    }
                }
                let flag = self.pending[0];
                let raw = &self.pending[5..];
                let payload = if flag & 1 != 0 {
                    let mut inflated = Vec::new();
                    flate2::read::GzDecoder::new(raw)
                        .take((MAX_INFLATED + 1) as u64)
                        .read_to_end(&mut inflated)
                        .map_err(|_| protocol_error())?;
                    if inflated.len() > MAX_INFLATED {
                        return Err(protocol_error());
                    }
                    inflated
                } else {
                    raw.to_vec()
                };
                self.pending.clear();
                self.expected = None;
                if flag & 2 != 0 {
                    self.ended = true;
                    frames.push(Frame::End(payload));
                } else {
                    frames.push(Frame::Data(payload));
                }
            }
            Ok(frames)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Checks framing only. The response session additionally requires an EOS trailer.
    pub fn finish(&self) -> Result<(), GatewayError> {
        if self.failed || !self.pending.is_empty() {
            Err(protocol_error())
        } else {
            Ok(())
        }
    }
}

pub fn envelope(payload: &[u8]) -> Result<Vec<u8>, GatewayError> {
    if payload.len() > MAX_FRAME {
        return Err(GatewayError::BadRequest {
            code: "devin_request_too_large",
            msg: "Devin request exceeds the frame size limit".into(),
        });
    }
    let mut bytes = Vec::with_capacity(5 + payload.len());
    bytes.push(0);
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

pub(super) fn encode_tool_id(id: &str) -> String {
    if id.is_empty()
        || (!id.starts_with(TOOL_ID_PREFIX)
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))
    {
        id.into()
    } else {
        format!("{TOOL_ID_PREFIX}{}", URL_SAFE_NO_PAD.encode(id))
    }
}

pub(super) fn decode_tool_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix(TOOL_ID_PREFIX)
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(rest)
        && let Ok(decoded) = String::from_utf8(bytes)
        && !decoded.is_empty()
        && encode_tool_id(&decoded) == id
    {
        return decoded;
    }
    id.into()
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientMetadata {
    #[prost(string, tag = "1")]
    pub client: String,
    #[prost(string, tag = "2")]
    pub version: String,
    #[prost(string, tag = "3")]
    pub token: String,
    #[prost(string, tag = "4")]
    pub locale: String,
    #[prost(string, tag = "5")]
    pub os: String,
    #[prost(string, tag = "7")]
    pub extension_version: String,
    #[prost(string, tag = "12")]
    pub extension: String,
    #[prost(string, tag = "31")]
    pub fingerprint: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ChatRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<ClientMetadata>,
    #[prost(string, tag = "2")]
    pub system: String,
    #[prost(message, repeated, tag = "3")]
    pub prompts: Vec<Prompt>,
    #[prost(uint32, tag = "7")]
    pub mode: u32,
    #[prost(message, optional, tag = "8")]
    pub completion: Option<Completion>,
    #[prost(message, repeated, tag = "10")]
    pub tools: Vec<Tool>,
    #[prost(message, optional, tag = "15")]
    pub thread: Option<Thread>,
    #[prost(string, tag = "16")]
    pub cascade_id: String,
    #[prost(uint32, tag = "20")]
    pub enabled: u32,
    #[prost(string, tag = "21")]
    pub model: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct Prompt {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(uint32, tag = "2")]
    pub source: u32,
    #[prost(string, optional, tag = "3")]
    pub content: Option<String>,
    #[prost(message, repeated, tag = "6")]
    pub calls: Vec<Call>,
    #[prost(string, tag = "7")]
    pub call_id: String,
    #[prost(message, repeated, tag = "10")]
    pub images: Vec<Image>,
    #[prost(string, tag = "11")]
    pub thinking: String,
    #[prost(bytes = "vec", tag = "12")]
    pub signature: Vec<u8>,
    #[prost(string, tag = "18")]
    pub signature_type: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct Call {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub arguments: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct Image {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, tag = "2")]
    pub mime_type: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct Completion {
    #[prost(uint32, tag = "1")]
    pub enabled: u32,
    #[prost(uint32, tag = "2")]
    pub max_tokens: u32,
    #[prost(uint32, tag = "3")]
    pub parameter_3: u32,
    #[prost(double, optional, tag = "5")]
    pub temperature: Option<f64>,
    #[prost(uint32, tag = "7")]
    pub top_k: u32,
    #[prost(double, optional, tag = "8")]
    pub top_p: Option<f64>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Tool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    // This is serialized JSON Schema, not google.protobuf.Struct.
    #[prost(bytes = "vec", tag = "3")]
    pub parameters: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Thread {
    #[prost(string, tag = "1")]
    pub session_id: String,
    #[prost(uint32, tag = "2")]
    pub turn_index: u32,
    #[prost(uint32, tag = "3")]
    pub mode: u32,
    #[prost(uint32, tag = "4")]
    pub user_boundary: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatResponse {
    #[prost(string, tag = "1")]
    pub id: String,
    // Field 2 has both integer and Timestamp encodings; neither is needed here.
    // Text may contain an incomplete UTF-8 sequence spanning response frames.
    #[prost(bytes = "vec", tag = "3")]
    pub text: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub delta_tokens: u32,
    #[prost(uint32, tag = "5")]
    pub stop_reason: u32,
    #[prost(message, repeated, tag = "6")]
    pub tools: Vec<CallDelta>,
    #[prost(message, optional, tag = "7")]
    pub usage: Option<UsageSnapshot>,
    #[prost(bytes = "vec", tag = "9")]
    pub thinking: Vec<u8>,
    #[prost(bytes = "vec", tag = "10")]
    pub signature: Vec<u8>,
    #[prost(string, tag = "17")]
    pub message_id: String,
    #[prost(string, tag = "21")]
    pub signature_type: String,
    #[prost(message, repeated, tag = "28")]
    pub dimensions: Vec<DimensionGroup>,
}
#[derive(Clone, PartialEq, Message)]
pub struct CallDelta {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(bytes = "vec", tag = "3")]
    pub arguments: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub invalid_json: Vec<u8>,
    #[prost(string, tag = "5")]
    pub invalid_json_error: String,
    #[prost(bool, tag = "6")]
    pub custom: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct UsageSnapshot {
    #[prost(uint32, optional, tag = "2")]
    pub input: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub output: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub cache_write: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub cache_read: Option<u32>,
    #[prost(uint32, tag = "6")]
    pub status: u32,
    // Header field 8 also exists as a legacy plain request ID; ignore it.
    #[prost(string, tag = "9")]
    pub model: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct DimensionGroup {
    #[prost(string, tag = "1")]
    pub title: String,
    #[prost(message, repeated, tag = "2")]
    pub metrics: Vec<DimensionMetric>,
}
#[derive(Clone, PartialEq, Message)]
pub struct DimensionMetric {
    #[prost(message, optional, tag = "4")]
    pub value: Option<DimensionValue>,
    #[prost(string, tag = "5")]
    pub key: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct DimensionValue {
    #[prost(float, tag = "2")]
    pub value: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn known_bytes_and_every_fragment_boundary() {
        let bytes = envelope(&[0x1a, 0x02, b'h', b'i']).unwrap();
        assert_eq!(bytes, [0, 0, 0, 0, 4, 0x1a, 2, b'h', b'i']);
        for split in 0..=bytes.len() {
            let mut d = Decoder::new();
            let mut frames = d.feed(&bytes[..split]).unwrap();
            frames.extend(d.feed(&bytes[split..]).unwrap());
            assert_eq!(frames, vec![Frame::Data(vec![0x1a, 2, b'h', b'i'])]);
            assert!(d.finish().is_ok());
        }
    }

    #[test]
    fn compressed_data_and_terminal_frames() {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(b"{}").unwrap();
        let payload = gzip.finish().unwrap();
        for flag in [1, 3] {
            let mut frame = envelope(&payload).unwrap();
            frame[0] = flag;
            let result = Decoder::new().feed(&frame).unwrap();
            assert_eq!(
                result,
                vec![if flag == 1 {
                    Frame::Data(b"{}".to_vec())
                } else {
                    Frame::End(b"{}".to_vec())
                }]
            );
        }
    }

    #[test]
    fn rejects_flags_lengths_partial_frames_and_trailing_data() {
        assert!(Decoder::new().feed(&[4, 0, 0, 0, 0]).is_err());
        assert!(Decoder::new().feed(&[0, 1, 0, 0, 1]).is_err());
        assert!(Decoder::new().feed(&[1, 0, 0, 0, 1, 0]).is_err());
        let mut d = Decoder::new();
        d.feed(&[0, 0]).unwrap();
        assert!(d.finish().is_err());
        let mut d = Decoder::new();
        d.feed(&[2, 0, 0, 0, 0]).unwrap();
        assert!(d.feed(&[0]).is_err());
    }

    #[test]
    fn rejects_gzip_expansion_bomb() {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let block = [0u8; 8192];
        for _ in 0..=(MAX_INFLATED / block.len()) {
            gzip.write_all(&block).unwrap();
        }
        let mut frame = envelope(&gzip.finish().unwrap()).unwrap();
        frame[0] = 1;
        assert!(Decoder::new().feed(&frame).is_err());
    }

    #[test]
    fn tool_ids_roundtrip_without_namespace_collisions() {
        for id in ["", "call-1", "space / 甲", "cpa_tid_v1_native"] {
            assert_eq!(decode_tool_id(&encode_tool_id(id)), id);
        }
        assert_eq!(decode_tool_id("cpa_tid_v1_%%%"), "cpa_tid_v1_%%%");
        assert_eq!(decode_tool_id("cpa_tid_v1_YWJj"), "cpa_tid_v1_YWJj");
    }
}
