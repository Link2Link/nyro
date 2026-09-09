use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nyro_core::logging::payload::{
    BODY_CAPTURE_LIMIT, BODY_HEAD_LIMIT, BODY_TAIL_LIMIT, BoundedPayloadCapture, CapturedPayload,
    HEADER_CAPTURE_LIMIT, capture_bytes, capture_header_map, capture_headers, capture_json,
};
use nyro_core::proxy::observability::{headers_to_json, redact_url_credentials};
use serde_json::{Value, json};

fn retained_raw(captured: &CapturedPayload) -> Vec<u8> {
    let body = captured.body.as_deref().expect("observed body");
    match captured.metadata["encoding"].as_str().unwrap() {
        "utf8" => body.as_bytes().to_vec(),
        "base64" => STANDARD.decode(body).unwrap(),
        other => panic!("unexpected encoding: {other}"),
    }
}

#[test]
fn chunk_boundaries_preserve_exact_head_tail() {
    let input: Vec<u8> = (0..BODY_CAPTURE_LIMIT * 3 + 173)
        .map(|index| (index % 251) as u8)
        .collect();
    let mut expected = input[..BODY_HEAD_LIMIT].to_vec();
    expected.extend_from_slice(&input[input.len() - BODY_TAIL_LIMIT..]);
    for chunk_size in [1, 3, 4093, BODY_TAIL_LIMIT - 1, BODY_CAPTURE_LIMIT * 2] {
        let mut capture = BoundedPayloadCapture::new();
        for chunk in input.chunks(chunk_size) {
            capture.push(chunk);
            assert!(capture.retained_bytes() <= BODY_CAPTURE_LIMIT);
        }
        assert_eq!(capture.total_observed_bytes(), input.len() as u64);
        let captured = capture.finish(true);
        assert_eq!(retained_raw(&captured), expected, "chunk size {chunk_size}");
        assert_eq!(captured.metadata["retained_bytes"], BODY_CAPTURE_LIMIT);
        assert_eq!(captured.metadata["head_bytes"], BODY_HEAD_LIMIT);
        assert_eq!(captured.metadata["tail_bytes"], BODY_TAIL_LIMIT);
        assert_eq!(captured.metadata["truncated"], true);
        assert_eq!(captured.metadata["complete"], true);
    }
}

#[test]
fn below_at_and_above_limit_are_unambiguous() {
    for size in [
        0,
        1,
        BODY_HEAD_LIMIT,
        BODY_HEAD_LIMIT + 1,
        BODY_CAPTURE_LIMIT - 1,
        BODY_CAPTURE_LIMIT,
        BODY_CAPTURE_LIMIT + 1,
    ] {
        let input = vec![b'a'; size];
        let captured = capture_bytes(&input, true);
        assert_eq!(
            captured.body.as_ref().unwrap().len(),
            size.min(BODY_CAPTURE_LIMIT)
        );
        assert_eq!(captured.metadata["total_observed_bytes"], size);
        assert_eq!(
            captured.metadata["retained_bytes"],
            size.min(BODY_CAPTURE_LIMIT)
        );
        assert_eq!(captured.metadata["head_bytes"], size.min(BODY_HEAD_LIMIT));
        assert_eq!(
            captured.metadata["tail_bytes"],
            size.saturating_sub(BODY_HEAD_LIMIT).min(BODY_TAIL_LIMIT)
        );
        assert_eq!(captured.metadata["truncated"], size > BODY_CAPTURE_LIMIT);
        assert_eq!(captured.metadata["encoding"], "utf8");
    }
}

#[test]
fn absent_explicit_empty_and_partial_are_distinct() {
    let absent = BoundedPayloadCapture::new().finish(false);
    assert!(absent.body.is_none());
    assert_eq!(absent.metadata["capture_state"], "absent");
    assert_eq!(absent.metadata["encoding"], "none");
    assert_eq!(absent.metadata["complete"], false);
    let absent_complete = BoundedPayloadCapture::new().finish(true);
    assert!(absent_complete.body.is_none());
    assert_eq!(absent_complete.metadata["complete"], true);
    let empty = capture_bytes(&[], true);
    assert_eq!(empty.body.as_deref(), Some(""));
    assert_eq!(empty.metadata["capture_state"], "empty");
    assert_eq!(empty.metadata["complete"], true);
    let partial = capture_bytes(b"observed until disconnect", false);
    assert_eq!(partial.metadata["capture_state"], "captured");
    assert_eq!(partial.metadata["complete"], false);
    assert_eq!(partial.metadata["truncated"], false);
    let partial_large = capture_bytes(&vec![b'x'; BODY_CAPTURE_LIMIT + 10], false);
    assert_eq!(partial_large.metadata["complete"], false);
    assert_eq!(partial_large.metadata["truncated"], true);
    assert_eq!(CapturedPayload::default().metadata, absent.metadata);
}

#[test]
fn valid_utf8_is_raw_and_chunk_independent() {
    let text = "  {\"raw\": \"雪🙂é\"}\n\n";
    for chunk_size in 1..=7 {
        let mut capture = BoundedPayloadCapture::new();
        for chunk in text.as_bytes().chunks(chunk_size) {
            capture.push(chunk);
        }
        let captured = capture.finish(true);
        assert_eq!(captured.body.as_deref(), Some(text));
        assert_eq!(captured.metadata["encoding"], "utf8");
    }
}

#[test]
fn incomplete_or_invalid_utf8_uses_reversible_base64() {
    for input in [
        &b"a\xffz"[..],
        &b"\xf0\x9f\x99"[..],
        &b"\xc0\xaf"[..],
        &b"\xe2x"[..],
    ] {
        let mut capture = BoundedPayloadCapture::new();
        for byte in input {
            capture.push(std::slice::from_ref(byte));
        }
        let captured = capture.finish(false);
        assert_eq!(captured.metadata["encoding"], "base64");
        assert_eq!(retained_raw(&captured), input);
    }
}

#[test]
fn utf8_split_edges_are_encoded_not_lossily_repaired() {
    let input = "€".repeat(BODY_CAPTURE_LIMIT / 3 + 200);
    let captured = capture_bytes(input.as_bytes(), true);
    assert_eq!(captured.metadata["encoding"], "base64");
    let mut expected = input.as_bytes()[..BODY_HEAD_LIMIT].to_vec();
    expected.extend_from_slice(&input.as_bytes()[input.len() - BODY_TAIL_LIMIT..]);
    assert_eq!(retained_raw(&captured), expected);
    assert_eq!(
        captured.body.unwrap().len(),
        BODY_CAPTURE_LIMIT.div_ceil(3) * 4
    );

    // No truncation: the internal head/tail boundary is not a missing section,
    // so a character spanning that boundary remains regular valid UTF-8.
    let exact = "€".repeat(BODY_CAPTURE_LIMIT / 3);
    let captured = capture_bytes(exact.as_bytes(), true);
    assert_eq!(captured.metadata["encoding"], "utf8");
    assert_eq!(captured.body.as_deref(), Some(exact.as_str()));
}

#[test]
fn invalid_bytes_in_omitted_middle_still_report_base64() {
    let mut input = vec![b'a'; BODY_CAPTURE_LIMIT + 100];
    input[BODY_HEAD_LIMIT + 5] = 0xff;
    let captured = capture_bytes(&input, true);
    assert_eq!(captured.metadata["encoding"], "base64");
    assert_eq!(retained_raw(&captured), vec![b'a'; BODY_CAPTURE_LIMIT]);
}

#[test]
fn json_writer_captures_exact_serialization_without_wire_truncation() {
    let value = json!({"text": "x\n雪".repeat(BODY_CAPTURE_LIMIT), "n": 42});
    let wire = serde_json::to_vec(&value).unwrap(); // authoritative wire fixture
    let captured = capture_json(&value, true);
    let reference = capture_bytes(&wire, true);
    assert_eq!(captured.body, reference.body);
    assert_eq!(captured.metadata, reference.metadata);
    assert_eq!(serde_json::from_slice::<Value>(&wire).unwrap(), value);
}

#[test]
fn four_independent_buffers_never_limit_forwarded_bytes() {
    let input = vec![b'x'; BODY_CAPTURE_LIMIT * 3 + 17];
    let mut captures: [BoundedPayloadCapture; 4] =
        std::array::from_fn(|_| BoundedPayloadCapture::new());
    let mut forwarded = Vec::new();
    for chunk in input.chunks(97) {
        forwarded.extend_from_slice(chunk);
        for capture in &mut captures {
            assert_eq!(capture.write(chunk).unwrap(), chunk.len());
        }
        assert!(
            captures
                .iter()
                .map(BoundedPayloadCapture::retained_bytes)
                .sum::<usize>()
                <= BODY_CAPTURE_LIMIT * 4
        );
    }
    assert_eq!(forwarded, input);
    let frozen = Arc::new(captures.into_iter().next().unwrap().finish(true));
    let builder_clone = Arc::clone(&frozen);
    assert!(Arc::ptr_eq(&frozen, &builder_clone));
}

#[test]
fn headers_are_bounded_valid_json_and_essential_fields_win() {
    let mut headers = HashMap::new();
    headers.insert("x-huge".to_string(), "x".repeat(HEADER_CAPTURE_LIMIT * 3));
    headers.insert("content-type".to_string(), "application/json".to_string());
    headers.insert("X-Request-Id".to_string(), "request-123".to_string());
    for index in 0..2000 {
        headers.insert(format!("x-noise-{index}"), "y".repeat(100));
    }
    let captured = capture_header_map(&headers);
    let serialized = captured.headers.as_deref().unwrap();
    assert!(serialized.len() <= HEADER_CAPTURE_LIMIT);
    let parsed: Value = serde_json::from_str(serialized).unwrap();
    assert_eq!(parsed["content-type"], "application/json");
    assert_eq!(parsed["x-request-id"], "request-123");
    assert!(parsed.get("x-huge").is_none());
    assert_eq!(captured.metadata["truncated"], true);
    assert_eq!(captured.metadata["retained_bytes"], serialized.len());
    let total: usize = headers
        .iter()
        .map(|(name, value)| name.len() + value.len())
        .sum();
    assert_eq!(captured.metadata["total_observed_bytes"], total);
    assert_eq!(headers["x-huge"].len(), HEADER_CAPTURE_LIMIT * 3);
}

#[test]
fn header_exact_limit_fits_mysql_text_and_next_byte_is_omitted() {
    assert_eq!(HEADER_CAPTURE_LIMIT, 65_535);
    // {"x":"..."} uses eight UTF-8 bytes outside the value.
    let mut headers = HashMap::from([("x".to_string(), "v".repeat(HEADER_CAPTURE_LIMIT - 8))]);
    let exact = capture_header_map(&headers);
    assert_eq!(exact.headers.as_ref().unwrap().len(), 65_535);
    assert_eq!(exact.metadata["truncated"], false);
    let parsed: Value = serde_json::from_str(exact.headers.as_deref().unwrap()).unwrap();
    assert_eq!(parsed["x"], headers["x"]);

    headers.get_mut("x").unwrap().push('v');
    let too_large = capture_header_map(&headers);
    assert_eq!(too_large.headers.as_deref(), Some("{}"));
    assert_eq!(too_large.metadata["truncated"], true);
    assert_eq!(too_large.metadata["omitted_headers"], 1);
}

#[test]
fn header_budget_accounts_for_escaping_before_copying() {
    let headers = HashMap::from([
        (
            "x-escaped".to_string(),
            "\u{0}".repeat(HEADER_CAPTURE_LIMIT / 2),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ]);
    let captured = capture_header_map(&headers);
    let parsed: Value = serde_json::from_str(captured.headers.as_deref().unwrap()).unwrap();
    assert!(parsed.get("x-escaped").is_none());
    assert_eq!(parsed["content-type"], "application/json");
    assert_eq!(captured.metadata["omitted_headers"], 1);
}

#[test]
fn auth_and_custom_secret_headers_redact_case_insensitively() {
    let secret_names = [
        "Authorization",
        "Proxy-Authorization",
        "Cookie",
        "Set-Cookie",
        "X-API-Key",
        "X-Goog-Api-Key",
        "OpenAI-Api-Key",
        "Anthropic-Api-Key",
        "X-Access-Token",
        "X-Password",
        "X-Client-Secret",
        "ApiKey",
        "X-Credential",
    ];
    let mut headers: HashMap<String, String> = secret_names
        .iter()
        .map(|name| {
            (
                name.to_string(),
                "must-not-leak".repeat(HEADER_CAPTURE_LIMIT),
            )
        })
        .collect();
    headers.insert("content-type".to_string(), "text/event-stream".to_string());
    let captured = capture_header_map(&headers);
    let serialized = captured.headers.as_deref().unwrap();
    assert!(!serialized.contains("must-not-leak"));
    let parsed: Value = serde_json::from_str(serialized).unwrap();
    for name in secret_names {
        assert_eq!(parsed[name.to_ascii_lowercase()], "***");
    }
    assert_eq!(captured.metadata["redacted_headers"], secret_names.len());
    assert_eq!(captured.metadata["truncated"], false);
}

#[test]
fn binary_and_empty_header_maps_remain_valid_json() {
    let mut headers = HeaderMap::new();
    headers.insert("x-binary", HeaderValue::from_bytes(&[0x80, 0xff]).unwrap());
    headers.insert(
        "authorization",
        HeaderValue::from_bytes(&[0x80, 0xff]).unwrap(),
    );
    let captured = capture_headers(&headers);
    let parsed: Value = serde_json::from_str(captured.headers.as_deref().unwrap()).unwrap();
    assert_eq!(parsed["x-binary"], "0x80ff");
    assert_eq!(parsed["authorization"], "***");
    assert_eq!(headers_to_json(&headers), captured.headers);
    let empty = capture_headers(&HeaderMap::new());
    assert_eq!(empty.headers.as_deref(), Some("{}"));
    assert_eq!(empty.metadata["capture_state"], "empty");
}

#[test]
fn url_credentials_and_broader_secret_query_keys_are_redacted() {
    let redacted = redact_url_credentials(
        "https://alice:password@host.test/v1?api_key=aaa&access_token=bbb&password=ccc&client_secret=ddd&%74oken=eee&safe=keep#access_token=fff",
    );
    for secret in [
        "alice",
        "password@",
        "aaa",
        "bbb",
        "ccc",
        "ddd",
        "eee",
        "fff",
    ] {
        assert!(!redacted.contains(secret), "leaked {secret}: {redacted}");
    }
    assert!(redacted.contains("safe=keep"));
    assert!(redacted.contains("/v1"));
}

#[test]
fn unicode_header_strings_remain_exact_and_bounded() {
    let headers = HashMap::from([
        (
            "X-Label".to_string(),
            "雪🙂é\\\"\\\\\n\t\r\u{0}".repeat(500),
        ),
        ("content-type".to_string(), "application/json".to_string()),
    ]);
    let captured = capture_header_map(&headers);
    let serialized = captured.headers.unwrap();
    assert!(serialized.len() <= HEADER_CAPTURE_LIMIT);
    let parsed: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed["x-label"], headers["X-Label"]);
    assert_eq!(captured.metadata["truncated"], false);
}

#[test]
fn utf8_pending_characters_across_many_chunks_match_standard_validation() {
    // Deterministic pseudo-random data exercises incomplete/invalid code points
    // without another production or test dependency.
    let mut seed = 0x1234_5678u32;
    for length in 0..128 {
        let mut input = Vec::new();
        for _ in 0..length {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            input.push((seed >> 24) as u8);
        }
        for chunk_size in 1..=5 {
            let mut capture = BoundedPayloadCapture::new();
            capture.push(&[]);
            for chunk in input.chunks(chunk_size) {
                capture.push(chunk);
            }
            let captured = capture.finish(true);
            let expected = if std::str::from_utf8(&input).is_ok() {
                "utf8"
            } else {
                "base64"
            };
            assert_eq!(captured.metadata["encoding"], expected);
            assert_eq!(retained_raw(&captured), input);
        }
    }
}

#[test]
fn malformed_urls_never_fall_through_with_raw_secrets() {
    for input in [
        "https://user:secret@[broken/?token=hidden",
        "/relative?password=hidden",
        "not a URL?api_key=hidden",
        "mailto:secret@example.com?token=hidden",
    ] {
        assert_eq!(redact_url_credentials(input), "[redacted invalid URL]");
    }
}
