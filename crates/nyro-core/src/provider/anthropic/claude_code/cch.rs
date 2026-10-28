//! Claude Code `cch` body attestation.
//!
//! Native Claude Code (2.1.220+) talking to first-party `api.anthropic.com`
//! embeds `cch=00000;` in the `x-anthropic-billing-header` system block, then
//! replaces those five digits with a hash of the final request bytes:
//!
//! ```text
//! cch = format!("{:05x}", xxh64(normalize(body_with_cch_zeroed), SEED) & 0xFFFFF)
//! ```
//!
//! `normalize` works on the raw bytes (no re-serialization): every `"model"`
//! string value is emptied and the dispatch-only members `max_tokens`,
//! `fallbacks` and `fallback_credit_token` are removed at any depth. Ported
//! from CLIProxyAPI `internal/runtime/executor/claude_signing.go`, whose
//! known vectors (captured from Claude Code 2.1.220) are pinned in the tests.

const CCH_SEED: u64 = 0x4D65_9218_E32A_3268;
const CCH_LEN: usize = 5;
pub(crate) const CCH_PLACEHOLDER: &str = "cch=00000;";
pub(crate) const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

// ── XXH64 ────────────────────────────────────────────────────────────────────

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("8-byte slice"))
}

fn read_u32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes(
        bytes[..4].try_into().expect("4-byte slice"),
    ))
}

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

/// One-shot XXH64 (the reference algorithm, little-endian lanes).
pub(crate) fn xxh64(input: &[u8], seed: u64) -> u64 {
    let len = input.len();
    let mut rest = input;
    let mut h = if len >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while rest.len() >= 32 {
            v1 = round(v1, read_u64(&rest[0..]));
            v2 = round(v2, read_u64(&rest[8..]));
            v3 = round(v3, read_u64(&rest[16..]));
            v4 = round(v4, read_u64(&rest[24..]));
            rest = &rest[32..];
        }
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        merge_round(h, v4)
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len as u64);
    while rest.len() >= 8 {
        h ^= round(0, read_u64(rest));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        h ^= read_u32(rest).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = &rest[4..];
    }
    for &byte in rest {
        h ^= u64::from(byte).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^= h >> 32;
    h
}

// ── Raw JSON scanner ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Edit {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<Edit>,
}

type ScanResult<T> = Result<T, String>;

impl<'a> Scanner<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            edits: Vec::new(),
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.body.len()
            && matches!(self.body[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }

    fn consume(&mut self, ch: u8) -> bool {
        if self.pos < self.body.len() && self.body[self.pos] == ch {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn add_edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push(Edit { start, end });
        }
    }

    /// Returns `(start, end)` of the string literal including both quotes.
    fn parse_string(&mut self) -> ScanResult<(usize, usize)> {
        if self.pos >= self.body.len() || self.body[self.pos] != b'"' {
            return Err(format!("missing JSON string at byte {}", self.pos));
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(format!("unterminated JSON string at byte {start}"))
    }

    fn parse_value(&mut self, collect: bool) -> ScanResult<()> {
        self.skip_ws();
        if self.pos >= self.body.len() {
            return Err(format!("missing JSON value at byte {}", self.pos));
        }
        match self.body[self.pos] {
            b'{' => self.parse_object(collect),
            b'[' => self.parse_array(collect),
            b'"' => self.parse_string().map(|_| ()),
            _ => {
                let start = self.pos;
                while self.pos < self.body.len()
                    && !matches!(
                        self.body[self.pos],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                    )
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(format!("missing JSON value at byte {start}"));
                }
                Ok(())
            }
        }
    }

    fn parse_object(&mut self, collect: bool) -> ScanResult<()> {
        self.pos += 1;
        self.skip_ws();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members: Vec<Member> = Vec::new();
        let mut comma_before = None;
        loop {
            self.skip_ws();
            let member_start = self.pos;
            let (key_start, key_end) = self.parse_string()?;
            self.skip_ws();
            if !self.consume(b':') {
                return Err(format!("missing object colon at byte {}", self.pos));
            }
            self.skip_ws();
            let key = &self.body[key_start..key_end];
            let excluded = collect && is_excluded_key(key);
            if collect
                && key == b"\"model\""
                && self.pos < self.body.len()
                && self.body[self.pos] == b'"'
            {
                let (value_start, value_end) = self.parse_string()?;
                self.add_edit(value_start + 1, value_end - 1);
            } else {
                self.parse_value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.skip_ws();
            let comma_after = if self.consume(b',') {
                Some(self.pos - 1)
            } else {
                None
            };
            members.push(Member {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.consume(b'}') {
                return Err(format!("missing object end at byte {}", self.pos));
            }
            break;
        }
        if collect {
            self.add_excluded_member_edits(&members);
        }
        Ok(())
    }

    fn parse_array(&mut self, collect: bool) -> ScanResult<()> {
        self.pos += 1;
        self.skip_ws();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(collect)?;
            self.skip_ws();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(format!("missing array end at byte {}", self.pos));
            }
            return Ok(());
        }
    }

    fn add_excluded_member_edits(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            if end + 1 < members.len() {
                let after = members[end].comma_after.map_or(members[end].end, |c| c + 1);
                self.add_edit(members[start].start, after);
            } else if start > 0 && end > start {
                // Claude Code 2.1.220 leaves the preceding comma in its hash
                // view when an object ends with several dispatch members.
                self.add_edit(members[start].start, members[end].end);
            } else if start > 0 {
                let from = members[start].comma_before.unwrap_or(members[start].start);
                self.add_edit(from, members[end].end);
            } else {
                self.add_edit(members[start].start, members[end].end);
            }
            start = end + 1;
        }
    }
}

fn is_excluded_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
    )
}

/// Build the CCH hash view without re-serializing the JSON.
fn normalize(body: &[u8]) -> ScanResult<Vec<u8>> {
    let mut scanner = Scanner::new(body);
    scanner.parse_value(true)?;
    scanner.skip_ws();
    if scanner.pos != body.len() {
        return Err(format!("unexpected JSON data at byte {}", scanner.pos));
    }
    let mut edits = scanner.edits;
    edits.sort_by_key(|edit| edit.start);
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for edit in edits {
        if edit.start < last || edit.end > body.len() {
            return Err(format!(
                "overlapping CCH normalization edit at byte {}",
                edit.start
            ));
        }
        out.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    out.extend_from_slice(&body[last..]);
    Ok(out)
}

// ── Locating `system[0].text` in raw bytes ───────────────────────────────────

/// Byte span `(start, end)` (quotes included) of `system[0].text` when it is a
/// string, located structurally so that copies of the billing text inside
/// `messages` can never be mistaken for the real one.
fn billing_text_span(body: &[u8]) -> Option<(usize, usize)> {
    let mut scanner = Scanner::new(body);
    scanner.skip_ws();
    let (system_start, _) = find_member(&mut scanner, b"\"system\"")?;
    scanner.pos = system_start;
    scanner.skip_ws();
    if !scanner.consume(b'[') {
        return None;
    }
    scanner.skip_ws();
    if scanner.pos >= body.len() || body[scanner.pos] != b'{' {
        return None;
    }
    let (text_start, _) = find_member(&mut scanner, b"\"text\"")?;
    scanner.pos = text_start;
    let (start, end) = scanner.parse_string().ok()?;
    Some((start, end))
}

/// With `scanner.pos` at an object's `{`, return the value span of `key`.
fn find_member(scanner: &mut Scanner<'_>, key: &[u8]) -> Option<(usize, usize)> {
    if !scanner.consume(b'{') {
        return None;
    }
    loop {
        scanner.skip_ws();
        if scanner.consume(b'}') {
            return None;
        }
        let (key_start, key_end) = scanner.parse_string().ok()?;
        scanner.skip_ws();
        if !scanner.consume(b':') {
            return None;
        }
        scanner.skip_ws();
        let value_start = scanner.pos;
        scanner.parse_value(false).ok()?;
        let value_end = scanner.pos;
        if &scanner.body[key_start..key_end] == key {
            return Some((value_start, value_end));
        }
        scanner.skip_ws();
        if scanner.consume(b',') {
            continue;
        }
        return None;
    }
}

/// Absolute offset of the five CCH digits inside `system[0].text`, when that
/// block is a billing header carrying `cch=<5 lowercase hex>;`.
fn cch_digits_offset(body: &[u8]) -> Option<usize> {
    let (start, end) = billing_text_span(body)?;
    let raw = &body[start..end];
    let prefix = format!("\"{BILLING_HEADER_PREFIX}");
    if !raw.starts_with(prefix.as_bytes()) {
        return None;
    }
    let mut from = 0;
    while let Some(rel) = find(&raw[from..], b"cch=") {
        let digits = from + rel + 4;
        let digits_end = digits + CCH_LEN;
        if digits_end < raw.len()
            && raw[digits_end] == b';'
            && raw[digits..digits_end]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Some(start + digits);
        }
        from = from + rel + 4;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Replace the CCH digits of the final serialized body with its attestation.
///
/// Bodies without a signable billing header are returned unchanged; only the
/// five digits ever change, so the signature covers exactly the bytes sent.
pub(crate) fn sign_body(body: &[u8]) -> Result<Vec<u8>, String> {
    let Some(offset) = cch_digits_offset(body) else {
        return Ok(body.to_vec());
    };
    let mut unsigned = body.to_vec();
    unsigned[offset..offset + CCH_LEN].copy_from_slice(b"00000");
    let normalized = normalize(&unsigned)?;
    let cch = format!("{:05x}", xxh64(&normalized, CCH_SEED) & 0xF_FFFF);
    unsigned[offset..offset + CCH_LEN].copy_from_slice(cch.as_bytes());
    Ok(unsigned)
}

/// Whether `body` carries a billing header whose `cch` is the correct
/// attestation of exactly these bytes (what Anthropic recomputes).
pub fn verify_body(body: &[u8]) -> bool {
    let Some(offset) = cch_digits_offset(body) else {
        return false;
    };
    let mut zeroed = body.to_vec();
    zeroed[offset..offset + CCH_LEN].copy_from_slice(b"00000");
    sign_body(&zeroed).is_ok_and(|resigned| resigned == body)
}

/// Insert ` cch=00000;` right after `cc_entrypoint=…;` when a billing header
/// lacks one (a native client behind a non-first-party base URL omits it).
pub(crate) fn ensure_placeholder(billing_text: &str) -> Option<String> {
    if !billing_text.starts_with(BILLING_HEADER_PREFIX) || billing_text.contains("cch=") {
        return None;
    }
    let entry = billing_text.find("cc_entrypoint=")?;
    let semi = billing_text[entry..].find(';')?;
    let insert_at = entry + semi + 1;
    let mut out = String::with_capacity(billing_text.len() + CCH_PLACEHOLDER.len() + 1);
    out.push_str(&billing_text[..insert_at]);
    out.push(' ');
    out.push_str(CCH_PLACEHOLDER);
    out.push_str(&billing_text[insert_at..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values produced by github.com/pierrec/xxHash/xxHash64 v0.1.5,
    // the implementation CLIProxyAPI signs with.
    #[test]
    fn xxh64_matches_reference_vectors() {
        let seed = CCH_SEED;
        let long = b"The quick brown fox jumps over the lazy dog, repeatedly, to exceed 32 bytes!";
        let cases: &[(&[u8], u64, u64)] = &[
            (b"", 0, 0xef46_db37_51d8_e999),
            (b"", seed, 0xb8b3_0e7d_e65b_46c5),
            (b"a", 0, 0xd24e_c4f1_a98c_6e5b),
            (b"a", seed, 0xde28_ae28_b0e0_7bb2),
            (b"abc", 0, 0x44bc_2cf5_ad77_0999),
            (b"abc", seed, 0xdfc4_f4d6_9136_99b6),
            (
                b"0123456789abcdef0123456789abcdef",
                0,
                0x642a_9495_8e71_e6c5,
            ),
            (
                b"0123456789abcdef0123456789abcdef",
                seed,
                0x7f82_b050_ec99_4b30,
            ),
            (long, 0, 0x4229_d51a_51e8_b28d),
            (long, seed, 0x0f55_9d4a_e134_ec96),
        ];
        for (input, seed, want) in cases {
            assert_eq!(
                xxh64(input, *seed),
                *want,
                "input={:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    const BASE: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

    fn cch_of(body: &str) -> String {
        let signed = sign_body(body.as_bytes()).unwrap();
        let signed = String::from_utf8(signed).unwrap();
        let value: serde_json::Value = serde_json::from_str(&signed).unwrap();
        let text = value["system"][0]["text"].as_str().unwrap().to_string();
        let idx = text.find("cch=").unwrap();
        text[idx + 4..idx + 9].to_string()
    }

    // Known vectors from CLIProxyAPI claude_signing_test.go (Claude Code 2.1.220).
    #[test]
    fn sign_matches_claude_code_known_vectors() {
        let r = |from: &str, to: &str| BASE.replacen(from, to, 1);
        let cases: Vec<(String, &str)> = vec![
            (BASE.to_string(), "7ee87"),
            (r(r#""model":"model-a""#, r#""model":"model-b""#), "7ee87"),
            (r(r#""max_tokens":1"#, r#""max_tokens":2"#), "7ee87"),
            (r(r#""text":"x""#, r#""text":"y""#), "b9cc8"),
            (r(r#""system-x""#, r#""system-y""#), "a30d3"),
            (r(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#), "7a89d"),
            (
                r(
                    r#""thinking":{"type":"adaptive","display":"omitted"}"#,
                    r#""thinking":{"type":"disabled"}"#,
                ),
                "7205c",
            ),
            (
                r(
                    r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#,
                    r#""context_management":{"edits":[]}"#,
                ),
                "05073",
            ),
            (r(r#""effort":"high""#, r#""effort":"low""#), "12366"),
            (r(r#""stream":true"#, r#""stream":false"#), "60400"),
            (
                r(
                    r#""tools":[]"#,
                    r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#,
                ),
                "3d78d",
            ),
            (r(r#""stream":true}"#, r#""stream":true,"extra_top":"extra"}"#), "2d622"),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","model":"a"}"#,
                ),
                "0601b",
            ),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":2}"#,
                ),
                "7ee87",
            ),
            (
                r(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#),
                "7ee87",
            ),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#,
                ),
                "4589b",
            ),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#,
                ),
                "2d312",
            ),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#,
                ),
                "0601b",
            ),
            (
                r(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","plain":"a"}"#,
                ),
                "8d74c",
            ),
            (
                r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#.to_string(),
                "f2edb",
            ),
            (
                r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#.to_string(),
                "e5b6c",
            ),
        ];
        for (body, want) in cases {
            assert_eq!(cch_of(&body), want, "body: {body}");
        }
    }

    #[test]
    fn normalize_preserves_raw_json_shape() {
        let cases = [
            (r#"{"model":"claude","keep":1}"#, r#"{"model":"","keep":1}"#),
            (r#"{"max_tokens":1,"keep":2}"#, r#"{"keep":2}"#),
            (
                r#"{"keep":1,"fallbacks":[{"model":"x"}],"tail":2}"#,
                r#"{"keep":1,"tail":2}"#,
            ),
            (
                r#"{"keep":1,"fallback_credit_token":"secret"}"#,
                r#"{"keep":1}"#,
            ),
            (
                r#"{"max_tokens":1,"fallbacks":[],"fallback_credit_token":"secret"}"#,
                r#"{}"#,
            ),
            (
                r#"{"keep":1,"max_tokens":1,"fallbacks":[],"tail":2}"#,
                r#"{"keep":1,"tail":2}"#,
            ),
            (
                r#"{"keep":1,"max_tokens":1,"fallbacks":[]}"#,
                r#"{"keep":1,}"#,
            ),
            (
                r#"{"outer":{"model":"x","max_tokens":1,"keep":"y"}}"#,
                r#"{"outer":{"model":"","keep":"y"}}"#,
            ),
            (
                r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
                r#"{"text":"literal \"model\":\"x\" and \"max_tokens\":1"}"#,
            ),
        ];
        for (input, want) in cases {
            let got = normalize(input.as_bytes()).unwrap();
            assert_eq!(String::from_utf8(got).unwrap(), want, "input: {input}");
        }
    }

    #[test]
    fn signing_changes_only_the_cch_digits_and_ignores_message_copies() {
        // A literal billing header inside a message must not be signed.
        let body = r#"{"messages":[{"role":"user","content":"x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=00000;"}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=cli; cch=00000;"}]}"#;
        let signed = String::from_utf8(sign_body(body.as_bytes()).unwrap()).unwrap();
        assert!(signed.contains(
            r#""content":"x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=00000;""#
        ));
        assert_eq!(signed.len(), body.len());
        let value: serde_json::Value = serde_json::from_str(&signed).unwrap();
        assert!(
            !value["system"][0]["text"]
                .as_str()
                .unwrap()
                .contains("cch=00000;")
        );
    }

    #[test]
    fn bodies_without_billing_header_are_untouched() {
        let body = br#"{"system":[{"type":"text","text":"hello cch=00000;"}]}"#;
        assert_eq!(sign_body(body).unwrap(), body.to_vec());
        let body = br#"{"system":"x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=00000;"}"#;
        assert_eq!(sign_body(body).unwrap(), body.to_vec());
    }

    #[test]
    fn placeholder_is_inserted_after_entrypoint() {
        assert_eq!(
            ensure_placeholder("x-anthropic-billing-header: cc_version=2.1.220.abc; cc_entrypoint=cli; cc_workload=x;")
                .as_deref(),
            Some("x-anthropic-billing-header: cc_version=2.1.220.abc; cc_entrypoint=cli; cch=00000; cc_workload=x;")
        );
        assert_eq!(
            ensure_placeholder(
                "x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=abcde;"
            ),
            None
        );
        assert_eq!(ensure_placeholder("You are Claude Code"), None);
    }
}
