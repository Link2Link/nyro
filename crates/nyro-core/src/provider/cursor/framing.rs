//! Incremental Connect envelopes: one flag byte and a big-endian u32 length.
use super::errors::CursorError;
use flate2::bufread::GzDecoder;
use serde_json::Value;
use std::io::Read;

pub const MAX_COMPRESSED_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_INFLATED_FRAME: usize = 8 * 1024 * 1024;
pub const MAX_BLOB_STORE: usize = 32 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum Frame {
    Data(Vec<u8>),
    End,
}

#[derive(Default, Debug)]
pub struct ConnectDecoder {
    buffer: Vec<u8>,
    expected: Option<usize>,
    ended: bool,
}
impl ConnectDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    /// Does not buffer an entire HTTP chunk: each advertised length is checked
    /// before allocating its body, including when many envelopes arrive together.
    pub fn push(&mut self, mut data: &[u8]) -> Result<Vec<Frame>, CursorError> {
        let mut frames = Vec::new();
        while !data.is_empty() {
            if self.ended {
                return Err(CursorError::Protocol(
                    "data after Connect end envelope".into(),
                ));
            }
            let target = self.expected.unwrap_or(5);
            let take = (target - self.buffer.len()).min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() != target {
                continue;
            }
            if self.expected.is_none() {
                let flags = self.buffer[0];
                if flags & !3 != 0 {
                    return Err(CursorError::Protocol("reserved Connect flags".into()));
                }
                let size = u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
                let limit = if flags & 1 != 0 {
                    MAX_COMPRESSED_FRAME
                } else {
                    MAX_INFLATED_FRAME
                };
                if size > limit {
                    return Err(CursorError::LimitExceeded {
                        resource: "frame",
                        limit,
                    });
                }
                self.expected = Some(size + 5);
                if size != 0 {
                    continue;
                }
            }
            let flags = self.buffer[0];
            let payload = if flags & 1 != 0 {
                let mut decoder = GzDecoder::new(&self.buffer[5..]);
                let mut out = Vec::new();
                (&mut decoder)
                    .take(MAX_INFLATED_FRAME as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| CursorError::Compression(e.to_string()))?;
                if out.len() > MAX_INFLATED_FRAME {
                    return Err(CursorError::LimitExceeded {
                        resource: "inflated frame",
                        limit: MAX_INFLATED_FRAME,
                    });
                }
                if !decoder.into_inner().is_empty() {
                    return Err(CursorError::Compression(
                        "trailing bytes or multiple gzip members".into(),
                    ));
                }
                out
            } else {
                self.buffer[5..].to_vec()
            };
            self.buffer.clear();
            self.expected = None;
            if flags & 2 != 0 {
                validate_end_envelope(&payload)?;
                self.ended = true;
                frames.push(Frame::End);
            } else {
                frames.push(Frame::Data(payload));
            }
        }
        Ok(frames)
    }
    pub fn finish(&self) -> Result<(), CursorError> {
        if !self.buffer.is_empty() {
            Err(CursorError::UnexpectedEof)
        } else {
            Ok(())
        }
    }
    pub fn saw_end(&self) -> bool {
        self.ended
    }
}

pub fn encode_frame(body: &[u8]) -> Result<Vec<u8>, CursorError> {
    if body.len() > MAX_INFLATED_FRAME {
        return Err(CursorError::LimitExceeded {
            resource: "outgoing frame",
            limit: MAX_INFLATED_FRAME,
        });
    }
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(0);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

fn validate_end_envelope(bytes: &[u8]) -> Result<(), CursorError> {
    let value: Value = serde_json::from_slice(bytes)?;
    let obj = value
        .as_object()
        .ok_or_else(|| CursorError::Protocol("Connect end must be an object".into()))?;
    if let Some(error) = obj.get("error") {
        let error = error
            .as_object()
            .ok_or_else(|| CursorError::Protocol("invalid Connect error".into()))?;
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| CursorError::Protocol("Connect error missing code".into()))?;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream error");
        return Err(CursorError::Upstream {
            code: code.into(),
            message: message.into(),
        });
    }
    if let Some(metadata) = obj.get("metadata") {
        let metadata = metadata
            .as_object()
            .ok_or_else(|| CursorError::Protocol("invalid Connect metadata".into()))?;
        let mut pairs = Vec::new();
        for (name, values) in metadata {
            let values = values
                .as_array()
                .ok_or_else(|| CursorError::Protocol("invalid Connect metadata values".into()))?;
            for value in values {
                pairs.push((
                    name.as_str(),
                    value
                        .as_str()
                        .ok_or_else(|| CursorError::Protocol("invalid metadata value".into()))?,
                ));
            }
        }
        validate_trailers(pairs)?;
    }
    Ok(())
}

/// Checks transport trailers without importing HTTP or transport dependencies.
/// Returns true only when a transport status explicitly confirms success.
pub fn validate_trailers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<bool, CursorError> {
    let mut status = None;
    let mut message = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("grpc-status") {
            if status.replace(value).is_some() {
                return Err(CursorError::Protocol("duplicate grpc-status".into()));
            }
        } else if name.eq_ignore_ascii_case("grpc-message") {
            message = Some(value);
        } else if name.eq_ignore_ascii_case("connect-error-code") {
            return Err(CursorError::Upstream {
                code: value.into(),
                message: "Connect transport error".into(),
            });
        }
    }
    match status {
        Some("0") => Ok(true),
        Some(code) if code.parse::<u32>().is_ok() => Err(CursorError::Upstream {
            code: code.into(),
            message: message.unwrap_or("gRPC transport error").into(),
        }),
        Some(_) => Err(CursorError::Protocol("invalid grpc-status".into())),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn compressed(body: &[u8], flags: u8) -> Vec<u8> {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(body).unwrap();
        let body = gzip.finish().unwrap();
        let mut frame = vec![flags];
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend(body);
        frame
    }
    #[test]
    fn split_every_byte_and_sticky_frames() {
        let bytes = [encode_frame(b"one").unwrap(), encode_frame(b"two").unwrap()].concat();
        for split in 0..=bytes.len() {
            let mut d = ConnectDecoder::new();
            let mut frames = d.push(&bytes[..split]).unwrap();
            frames.extend(d.push(&bytes[split..]).unwrap());
            assert_eq!(
                frames,
                vec![Frame::Data(b"one".to_vec()), Frame::Data(b"two".to_vec())]
            );
            d.finish().unwrap();
        }
        let mut d = ConnectDecoder::new();
        let mut frames = Vec::new();
        for b in bytes {
            frames.extend(d.push(&[b]).unwrap());
        }
        assert_eq!(frames.len(), 2);
    }
    #[test]
    fn gzip_end_errors_and_corruption() {
        assert_eq!(
            ConnectDecoder::new()
                .push(&compressed(b"hello", 1))
                .unwrap(),
            vec![Frame::Data(b"hello".to_vec())]
        );
        let err = compressed(
            br#"{"error":{"code":"resource_exhausted","message":"quota"}}"#,
            3,
        );
        assert!(matches!(
            ConnectDecoder::new().push(&err),
            Err(CursorError::Upstream { .. })
        ));
        let mut bad = compressed(b"hi", 1);
        bad[6] ^= 0xff;
        assert!(matches!(
            ConnectDecoder::new().push(&bad),
            Err(CursorError::Compression(_))
        ));
    }
    #[test]
    fn truncated_oversized_reserved_and_bomb() {
        let mut d = ConnectDecoder::new();
        d.push(&[0, 0]).unwrap();
        assert_eq!(d.finish(), Err(CursorError::UnexpectedEof));
        let mut head = vec![1];
        head.extend_from_slice(&((MAX_COMPRESSED_FRAME + 1) as u32).to_be_bytes());
        assert!(matches!(
            ConnectDecoder::new().push(&head),
            Err(CursorError::LimitExceeded { .. })
        ));
        assert!(ConnectDecoder::new().push(&[4, 0, 0, 0, 0]).is_err());
        assert!(matches!(
            ConnectDecoder::new().push(&compressed(&vec![0; MAX_INFLATED_FRAME + 1], 1)),
            Err(CursorError::LimitExceeded { .. })
        ));
    }
    #[test]
    fn terminal_cannot_hide_trailing_data() {
        let bytes = [
            vec![2, 0, 0, 0, 2, b'{', b'}'],
            encode_frame(b"late").unwrap(),
        ]
        .concat();
        assert!(ConnectDecoder::new().push(&bytes).is_err());
        assert!(validate_trailers([("grpc-status", "13"), ("grpc-message", "failure")]).is_err());
        assert!(validate_trailers([("grpc-status", "0")]).unwrap());
    }
}
