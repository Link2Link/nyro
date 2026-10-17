//! Local token estimates for Kiro, which reports no per-token counts.
//!
//! The method follows opencodex's Kiro adapter (`lib/token-estimate.ts` and
//! `adapters/kiro/usage.ts`): characters are counted per script and converted
//! with a fixed ratio each, the prompt is estimated from the serialized
//! conversation payload, and every conversation entry is charged a flat
//! structural cost. The constants were fitted there against recorded Kiro
//! traffic, not against this gateway, so treat the results as estimates.
use serde_json::Value;

/// Latin text, code and JSON: characters per token.
const LATIN_CHARS_PER_TOKEN: f64 = 2.8;
/// Hangul, Han and kana: characters per token.
const CJK_CHARS_PER_TOKEN: f64 = 1.5;
/// The wire charges more than the plain text count for Latin content (JSON
/// framing and escaping). Applied to the Latin share only.
const LATIN_WIRE_EXPANSION: f64 = 1.2;
/// Flat structural cost of one conversation entry (keys and role framing).
pub(super) const ENTRY_FRAMING_TOKENS: u32 = 27;
/// Flat token cost charged per inline image in the prompt estimate.
pub(super) const IMAGE_TOKENS: u32 = 1_600;

/// Characters of a text blob split by script.
///
/// Lengths are UTF-16 code units, matching the reference implementation, so
/// astral characters count as two Latin units (slightly high, the safe side).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CharCounts {
    latin: u64,
    cjk: u64,
}

fn is_cjk(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0xAC00..=0xD7A3
            | 0x1100..=0x11FF
            | 0x3130..=0x318F
            | 0x4E00..=0x9FFF
            | 0x3400..=0x4DBF
            | 0x3040..=0x30FF
    )
}

/// Tokens for one script bucket: zero when empty, otherwise at least one.
fn bucket(chars: u64, chars_per_token: f64) -> f64 {
    if chars == 0 {
        0.0
    } else {
        (chars as f64 / chars_per_token).ceil().max(1.0)
    }
}

impl CharCounts {
    pub(super) fn add(&mut self, text: &str) {
        for ch in text.chars() {
            if is_cjk(ch) {
                self.cjk += 1;
            } else {
                self.latin += ch.len_utf16() as u64;
            }
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.latin += other.latin;
        self.cjk += other.cjk;
    }

    /// Plain text estimate, continuous in the CJK share (no threshold).
    pub(super) fn tokens(self) -> u32 {
        if self.latin + self.cjk == 0 {
            return 0;
        }
        let estimate =
            self.latin as f64 / LATIN_CHARS_PER_TOKEN + self.cjk as f64 / CJK_CHARS_PER_TOKEN;
        estimate.ceil().max(1.0) as u32
    }

    /// Estimate for text that is sent on the wire: the Latin share is scaled
    /// by [`LATIN_WIRE_EXPANSION`], CJK is not.
    pub(super) fn wire_tokens(self) -> u32 {
        if self.latin + self.cjk == 0 {
            return 0;
        }
        let latin = bucket(self.latin, LATIN_CHARS_PER_TOKEN);
        let cjk = bucket(self.cjk, CJK_CHARS_PER_TOKEN);
        (latin * LATIN_WIRE_EXPANSION + cjk).ceil() as u32
    }
}

/// Accumulates the text the prompt estimate covers, joined with `\n` as the
/// reference implementation does.
#[derive(Default)]
struct Walk {
    counts: CharCounts,
    parts: u64,
    images: u64,
}

impl Walk {
    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.parts > 0 {
            self.counts.latin += 1;
        }
        self.parts += 1;
        self.counts.add(text);
    }

    /// Non-empty arrays (tools, tool results, tool uses) count as their JSON.
    fn push_json(&mut self, value: &Value) {
        if value.as_array().is_some_and(|items| !items.is_empty()) {
            self.push(&value.to_string());
        }
    }

    fn user(&mut self, message: &Value) {
        if let Some(text) = message["content"].as_str() {
            self.push(text);
        }
        self.images += message["images"].as_array().map_or(0, |i| i.len() as u64);
        let context = &message["userInputMessageContext"];
        self.push_json(&context["tools"]);
        self.push_json(&context["toolResults"]);
    }

    fn assistant(&mut self, message: &Value) {
        if let Some(text) = message["content"].as_str() {
            self.push(text);
        }
        self.push_json(&message["toolUses"]);
    }
}

/// Prompt estimate for the payload actually sent: `history` entries are the
/// wrapped `userInputMessage` / `assistantResponseMessage` values and
/// `current` is the inner current `userInputMessage`. System text is already
/// part of the first history entry. Images are charged flat, and only images
/// that are really in the payload count.
pub(super) fn payload_input_tokens(history: &[Value], current: &Value) -> u32 {
    let mut walk = Walk::default();
    for entry in history {
        walk.user(&entry["userInputMessage"]);
        walk.assistant(&entry["assistantResponseMessage"]);
    }
    walk.user(current);
    let entries = history.len() as u64 + 1;
    let total = u64::from(walk.counts.wire_tokens())
        + walk.images * u64::from(IMAGE_TOKENS)
        + entries * u64::from(ENTRY_FRAMING_TOKENS);
    u32::try_from(total).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn counts(text: &str) -> CharCounts {
        let mut counts = CharCounts::default();
        counts.add(text);
        counts
    }

    #[test]
    fn plain_estimate_uses_a_ratio_per_script() {
        assert_eq!(counts("").tokens(), 0);
        assert_eq!(counts("a").tokens(), 1);
        assert_eq!(counts("abcd").tokens(), 2, "4 / 2.8 rounds up");
        assert_eq!(counts("你").tokens(), 1);
        assert_eq!(counts("你好").tokens(), 2, "2 / 1.5 rounds up");
    }

    #[test]
    fn plain_estimate_is_continuous_in_the_cjk_share() {
        // 10 / 2.8 + 3 / 1.5 = 5.57
        assert_eq!(counts("abcdefghij你好吗").tokens(), 6);
        // One extra CJK character moves the estimate by a fraction, not a ratio switch.
        assert!(
            counts(&format!("{}你", "a".repeat(100))).tokens()
                <= counts(&"a".repeat(100)).tokens() + 1
        );
    }

    #[test]
    fn astral_characters_count_as_two_utf16_units() {
        assert_eq!(counts("😀"), CharCounts { latin: 2, cjk: 0 });
        assert_eq!(counts("😀😀").tokens(), 2);
    }

    #[test]
    fn wire_estimate_scales_only_the_latin_share() {
        assert_eq!(counts("").wire_tokens(), 0);
        // ceil(100 / 2.8) = 36, 36 * 1.2 = 43.2
        assert_eq!(counts(&"a".repeat(100)).wire_tokens(), 44);
        // ceil(30 / 1.5) = 20, not expanded
        assert_eq!(counts(&"你".repeat(30)).wire_tokens(), 20);
        // 36 * 1.2 + 20 = 63.2
        let mixed = format!("{}{}", "a".repeat(100), "你".repeat(30));
        assert_eq!(counts(&mixed).wire_tokens(), 64);
    }

    #[test]
    fn merge_adds_both_buckets() {
        let mut total = counts("ab");
        total.merge(counts("你"));
        assert_eq!(total, CharCounts { latin: 2, cjk: 1 });
    }

    #[test]
    fn payload_charges_text_and_per_entry_framing() {
        // "hello": ceil(5 / 2.8) = 2, * 1.2 = 2.4 -> 3; one entry -> 27.
        assert_eq!(payload_input_tokens(&[], &json!({"content": "hello"})), 30);
        // "ab\ncd" is 5 Latin units -> 3; two entries -> 54.
        let history = [json!({"userInputMessage": {"content": "ab"}})];
        assert_eq!(
            payload_input_tokens(&history, &json!({"content": "cd"})),
            57
        );
    }

    #[test]
    fn payload_charges_images_flat_and_counts_structured_parts() {
        // "hi": ceil(2 / 2.8) = 1, * 1.2 = 1.2 -> 2; two images; one entry.
        let current = json!({"content": "hi", "images": [{}, {}]});
        assert_eq!(
            payload_input_tokens(&[], &current),
            2 + 2 * IMAGE_TOKENS + ENTRY_FRAMING_TOKENS
        );

        let bare = json!({"content": "hi"});
        let with_tools = json!({
            "content": "hi",
            "userInputMessageContext": {"tools": [{"toolSpecification": {"name": "t"}}]}
        });
        assert!(payload_input_tokens(&[], &with_tools) > payload_input_tokens(&[], &bare));
        let assistant = [json!({"assistantResponseMessage": {
            "content": "ok", "toolUses": [{"toolUseId": "1", "name": "t", "input": {}}]
        }})];
        assert!(payload_input_tokens(&assistant, &bare) > payload_input_tokens(&[], &bare));
    }

    #[test]
    fn empty_and_missing_parts_are_ignored() {
        let current = json!({"content": "", "userInputMessageContext": {"tools": []}});
        assert_eq!(payload_input_tokens(&[], &current), ENTRY_FRAMING_TOKENS);
    }
}
