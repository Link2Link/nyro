use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::db::model_rating_prefixes::{
    ModelRatingEntry, canonical_model_prefix, longest_matching_entry,
};

/// One consolidated Artificial Analysis model entry representing the highest
/// reasoning effort tier of a base model family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AaModelRatingEntry {
    /// Canonical base key derived from the AA model slug (e.g. "claude-opus-5-5").
    pub canonical_key: String,
    /// Original AA slug of the selected highest-effort variant.
    pub aa_slug: String,
    /// Original AA display name (e.g. "Claude Opus 5.5 (Max, Default Fallback)").
    pub aa_name: String,
    /// Creator / lab name (e.g. "Anthropic").
    pub aa_creator: String,
    /// Selected highest reasoning effort label (e.g. "max", "xhigh", "high", "reasoning", "default").
    pub selected_effort: String,
    /// Raw floating-point Artificial Analysis Intelligence Index score.
    pub raw_score: f64,
    /// Rounded integer score in 0..=100 for compatibility with Nyro's 0-100 rating scale.
    pub score: i32,
    /// Release date reported by AA when present (e.g. "2026-08-18").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
}

/// Raw model item parsed from Artificial Analysis API (/api/v2/language/models/free).
#[derive(Debug, Clone, Deserialize)]
pub struct AaRawModel {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub model_creator: Option<AaRawCreator>,
    #[serde(default)]
    pub evaluations: Option<AaRawEvaluations>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AaRawCreator {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AaRawEvaluations {
    #[serde(default)]
    pub artificial_analysis_intelligence_index: Option<f64>,
}

/// User-configured explicit alias mapping a model prefix to an AA canonical key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AaModelAlias {
    pub model_prefix: String,
    pub aa_canonical_key: String,
    pub updated_at: String,
}

/// Source of an effective rating resolved for a concrete model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveRatingSource {
    Manual,
    AaAlias,
    AaAuto,
}

/// Effective resolved rating for any concrete model name (manual > AA alias > AA auto).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveModelRating {
    /// Key used for grouping (canonical manual prefix or AA canonical key).
    pub model_prefix: String,
    pub score: i32,
    pub raw_score: Option<f64>,
    pub updated_at: String,
    pub source: EffectiveRatingSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aa_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aa_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffortInfo {
    rank: u8,
    label: &'static str,
}

const EFFORT_MAX: EffortInfo = EffortInfo {
    rank: 80,
    label: "max",
};
const EFFORT_XHIGH: EffortInfo = EffortInfo {
    rank: 70,
    label: "xhigh",
};
const EFFORT_HIGH: EffortInfo = EffortInfo {
    rank: 60,
    label: "high",
};
const EFFORT_REASONING: EffortInfo = EffortInfo {
    rank: 50,
    label: "reasoning",
};
const EFFORT_MEDIUM: EffortInfo = EffortInfo {
    rank: 40,
    label: "medium",
};
const EFFORT_LOW: EffortInfo = EffortInfo {
    rank: 30,
    label: "low",
};
const EFFORT_MINIMAL: EffortInfo = EffortInfo {
    rank: 20,
    label: "minimal",
};
const EFFORT_NONE: EffortInfo = EffortInfo {
    rank: 10,
    label: "none",
};
const EFFORT_DEFAULT: EffortInfo = EffortInfo {
    rank: 0,
    label: "default",
};

/// Known OpenRouter / Ollama routing tags after a trailing ':' that are not model identities.
const STRIPPABLE_COLON_TAGS: &[&str] = &[
    "free", "exacto", "thinking", "extended", "nitro", "floor", "online", "beta", "latest",
];

/// Brand prefixes that may appear glued to a leading digit (e.g. "qwen3-5" vs "qwen-3-5").
const GLUED_BRAND_PREFIXES: &[&str] = &[
    "qwen",
    "llama",
    "gemma",
    "glm",
    "lfm",
    "olmo",
    "step",
    "solar",
    "minicpm",
    "mistral",
    "ministral",
    "devstral",
    "mixtral",
    "granite",
    "jamba",
    "exaone",
    "ling",
    "ring",
];

/// Vendor prefixes that AA sometimes prepends to slugs (e.g. "nvidia-nemotron-...").
const OPTIONAL_VENDOR_PREFIXES: &[&str] = &[
    "nvidia",
    "tencent",
    "alibaba",
    "meta",
    "google",
    "openai",
    "anthropic",
    "xai",
    "deepseek",
    "mistral",
    "moonshotai",
    "zhipuai",
    "minimax",
    "xiaomi",
    "amazon",
    "cohere",
];

/// Product-tier words that distinguish different models and MUST NEVER be stripped.
const PROTECTED_PRODUCT_TOKENS: &[&str] = &[
    "pro",
    "flash",
    "lite",
    "mini",
    "nano",
    "micro",
    "tiny",
    "small",
    "large",
    "ultra",
    "turbo",
    "fast",
    "codex",
    "spark",
    "omni",
    "vl",
    "vision",
    "code",
    "coder",
    "audio",
    "tts",
    "stt",
    "image",
    "embedding",
    "distill",
    "air",
    "plus",
    "speciale",
    "terminus",
    "multi-agent",
    "deep-think",
    "deep-research",
    "sol",
    "terra",
    "luna",
    "astra",
    "argon",
    "carbon",
    "hydrogen",
    "neon",
    "opus",
    "sonnet",
    "haiku",
    "fable",
    "scout",
    "maverick",
    "instant",
    "next",
    "open",
    "full",
    "super",
    "bolt",
    "lightning",
];

/// Strip channel namespace prefixes ("anthropic/claude-opus-5.5" -> "claude-opus-5.5"),
/// trailing non-identity colon tags (":free", ":exacto"), and parenthetical annotations
/// ("Claude Opus 5.5 (Max, Default Fallback)" -> "Claude Opus 5.5").
fn strip_outer_noise(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some((_, tail)) = s.rsplit_once('/')
        && !tail.trim().is_empty()
    {
        s = tail.trim();
    }
    if let Some((head, tag)) = s.rsplit_once(':')
        && STRIPPABLE_COLON_TAGS
            .iter()
            .any(|candidate| tag.trim().eq_ignore_ascii_case(candidate))
        && !head.trim().is_empty()
    {
        s = head.trim();
    }
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn tokenize_model_name(raw: &str) -> Vec<String> {
    let cleaned = strip_outer_noise(raw).to_lowercase();
    let bytes = cleaned.as_bytes();
    let mut tokens = Vec::new();
    let mut current = String::new();

    for (idx, ch) in cleaned.char_indices() {
        let is_numeric_dot = ch == '.'
            && idx > 0
            && bytes[idx - 1].is_ascii_digit()
            && bytes.get(idx + 1).is_some_and(u8::is_ascii_digit);
        if ch.is_whitespace() || ch == '-' || ch == '_' || is_numeric_dot {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_all_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_claude_tier(s: &str) -> bool {
    matches!(s, "opus" | "sonnet" | "haiku" | "fable")
}

fn canonicalize_tokens(mut tokens: Vec<String>) -> Vec<String> {
    if tokens.is_empty() {
        return tokens;
    }

    // R2: Split glued brand prefix + version digits on token 0 (e.g. qwen3 -> qwen, 3).
    if let Some(first) = tokens.first().cloned() {
        for brand in GLUED_BRAND_PREFIXES {
            if let Some(rest) = first.strip_prefix(brand)
                && is_all_ascii_digits(rest)
            {
                tokens[0] = (*brand).to_string();
                tokens.insert(1, rest.to_string());
                break;
            }
        }
    }

    // R1: Claude legacy compact versions and inverted tier/version order.
    if tokens.first().map(String::as_str) == Some("claude") {
        if tokens.len() >= 2 && tokens[1].len() == 2 && is_all_ascii_digits(&tokens[1]) {
            let first_digit = tokens[1][0..1].to_string();
            let second_digit = tokens[1][1..2].to_string();
            if matches!(first_digit.as_str(), "2" | "3" | "4" | "5" | "6") {
                tokens[1] = first_digit;
                tokens.insert(2, second_digit);
            }
        }

        if tokens.len() >= 3 && is_all_ascii_digits(&tokens[1]) && is_claude_tier(&tokens[2]) {
            tokens.swap(1, 2);
        } else if tokens.len() >= 4
            && is_all_ascii_digits(&tokens[1])
            && is_all_ascii_digits(&tokens[2])
            && is_claude_tier(&tokens[3])
        {
            let tier = tokens.remove(3);
            tokens.insert(1, tier);
        }
    }

    tokens
}

/// Produce the canonical normalized key for any model identifier or human display name.
pub fn normalize_model_key(raw: &str) -> String {
    canonicalize_tokens(tokenize_model_name(raw)).join("-")
}

fn extract_parenthetical_parts(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in name.chars() {
        match ch {
            '(' => {
                if depth > 0 {
                    current.push(ch);
                }
                depth += 1;
            }
            ')' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 && !current.trim().is_empty() {
                        parts.push(current.trim().to_lowercase());
                        current.clear();
                    }
                }
            }
            _ if depth > 0 => current.push(ch),
            _ => {}
        }
    }
    parts
}

fn parse_effort_token(phrase: &str) -> Option<EffortInfo> {
    let norm = phrase
        .trim()
        .to_lowercase()
        .replace(['_', ' '], "-")
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    match norm.as_str() {
        "max" | "maximum" | "max-effort" => Some(EFFORT_MAX),
        "xhigh" | "extra-high" | "very-high" => Some(EFFORT_XHIGH),
        "high" | "high-effort" => Some(EFFORT_HIGH),
        "reasoning" | "thinking" | "adaptive" | "extended" | "think" => Some(EFFORT_REASONING),
        "medium" | "med" | "medium-effort" => Some(EFFORT_MEDIUM),
        "low" | "low-effort" => Some(EFFORT_LOW),
        "minimal" | "xlow" | "extra-low" => Some(EFFORT_MINIMAL),
        "non-reasoning" | "no-reasoning" | "non-thinking" | "no-thinking" | "none" | "off" => {
            Some(EFFORT_NONE)
        }
        _ => None,
    }
}

fn parse_effort_from_name(name: &str) -> Option<EffortInfo> {
    let mut best: Option<EffortInfo> = None;
    for part in extract_parenthetical_parts(name) {
        for piece in part.split([',', '/', ';', '|']) {
            let trimmed = piece.trim();
            if let Some(info) = parse_effort_token(trimmed) {
                if best.is_none_or(|b| info.rank > b.rank) {
                    best = Some(info);
                }
                continue;
            }
            let words: Vec<&str> = trimmed.split_whitespace().collect();
            for window_len in [2usize, 1usize] {
                if words.len() < window_len {
                    continue;
                }
                for window in words.windows(window_len) {
                    let joined = window.join("-");
                    if let Some(info) = parse_effort_token(&joined)
                        && best.is_none_or(|b| info.rank > b.rank)
                    {
                        best = Some(info);
                    }
                }
            }
        }
    }
    best
}

fn strip_aa_slug_effort(
    slug_tokens: &[String],
    name_without_parens_tokens: &[String],
) -> (Vec<String>, Option<EffortInfo>) {
    let n = slug_tokens.len();
    if n < 2 {
        return (slug_tokens.to_vec(), None);
    }

    if n >= 3 {
        let pair = format!("{}-{}", slug_tokens[n - 2], slug_tokens[n - 1]);
        if let Some(info) = match pair.as_str() {
            "extra-high" => Some(EFFORT_XHIGH),
            "extra-low" => Some(EFFORT_MINIMAL),
            "non-reasoning" | "no-reasoning" | "non-thinking" | "no-thinking" => Some(EFFORT_NONE),
            _ => None,
        } {
            let name_ends_same = name_without_parens_tokens.len() >= 2
                && name_without_parens_tokens[name_without_parens_tokens.len() - 2]
                    == slug_tokens[n - 2]
                && name_without_parens_tokens[name_without_parens_tokens.len() - 1]
                    == slug_tokens[n - 1];
            if !name_ends_same {
                return (slug_tokens[..n - 2].to_vec(), Some(info));
            }
        }
    }

    let last = slug_tokens[n - 1].as_str();
    let effort = match last {
        "max" => Some(EFFORT_MAX),
        "xhigh" => Some(EFFORT_XHIGH),
        "high" => Some(EFFORT_HIGH),
        "reasoning" | "thinking" | "adaptive" => Some(EFFORT_REASONING),
        "medium" | "med" => Some(EFFORT_MEDIUM),
        "low" => Some(EFFORT_LOW),
        "minimal" | "xlow" => Some(EFFORT_MINIMAL),
        "none" => Some(EFFORT_NONE),
        _ => None,
    };

    if let Some(info) = effort {
        let name_ends_with_same = name_without_parens_tokens
            .last()
            .is_some_and(|token| token == last);
        if !name_ends_with_same {
            return (slug_tokens[..n - 1].to_vec(), Some(info));
        }
    }

    (slug_tokens.to_vec(), None)
}

fn is_month_word(s: &str) -> bool {
    matches!(
        s,
        "jan"
            | "feb"
            | "mar"
            | "apr"
            | "may"
            | "jun"
            | "jul"
            | "aug"
            | "sep"
            | "oct"
            | "nov"
            | "dec"
    )
}

fn is_date_like_suffix(tokens: &[String]) -> Option<usize> {
    let n = tokens.len();
    if n < 2 {
        return None;
    }
    let last = tokens[n - 1].as_str();

    if n >= 4
        && tokens[n - 3].len() == 4
        && is_all_ascii_digits(&tokens[n - 3])
        && tokens[n - 3].starts_with("20")
        && tokens[n - 2].len() == 2
        && is_all_ascii_digits(&tokens[n - 2])
        && last.len() == 2
        && is_all_ascii_digits(last)
    {
        return Some(3);
    }

    if n >= 3 {
        let prev = tokens[n - 2].as_str();
        let mm_yyyy = prev.len() == 2
            && is_all_ascii_digits(prev)
            && last.len() == 4
            && is_all_ascii_digits(last)
            && last.starts_with("20");
        let mm_dd = prev.len() == 2
            && is_all_ascii_digits(prev)
            && last.len() == 2
            && is_all_ascii_digits(last)
            && prev.parse::<u32>().is_ok_and(|m| (1..=12).contains(&m))
            && last.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d));
        let mon_year = is_month_word(prev)
            && (last.len() == 2 || (last.len() == 4 && last.starts_with("20")))
            && is_all_ascii_digits(last);
        if mm_yyyy || mm_dd || mon_year {
            return Some(2);
        }
    }

    if last.len() == 8 && is_all_ascii_digits(last) && last.starts_with("20") {
        return Some(1);
    }
    if last.len() == 4 && is_all_ascii_digits(last) {
        let a = last[0..2].parse::<u32>().unwrap_or(99);
        let b = last[2..4].parse::<u32>().unwrap_or(99);
        let valid_mmdd = (1..=12).contains(&a) && (1..=31).contains(&b);
        let valid_yymm = (23..=35).contains(&a) && (1..=12).contains(&b);
        if valid_mmdd || valid_yymm {
            return Some(1);
        }
    }
    if last.len() == 3 && is_all_ascii_digits(last) && last.starts_with('0') {
        return Some(1);
    }
    if last.len() >= 4
        && last.len() <= 5
        && is_month_word(&last[..3])
        && is_all_ascii_digits(&last[3..])
    {
        return Some(1);
    }
    None
}

fn is_preview_or_snapshot_slug(tokens: &[String]) -> bool {
    is_date_like_suffix(tokens).is_some()
        || tokens.iter().any(|t| {
            matches!(
                t.as_str(),
                "preview" | "beta" | "exp" | "experimental" | "alpha"
            )
        })
}

/// Consolidate raw AA language model entries into one fixed entry per base model,
/// strictly selecting the highest reasoning effort tier for each model.
pub fn consolidate_aa_models(raw_models: &[AaRawModel]) -> Vec<AaModelRatingEntry> {
    #[derive(Debug, Clone)]
    struct Candidate {
        entry: AaModelRatingEntry,
        effort_rank: u8,
        is_stable: bool,
        release_date: String,
        is_primary_slug: bool,
    }

    fn compare_candidates(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
        a.effort_rank
            .cmp(&b.effort_rank)
            .then_with(|| a.is_stable.cmp(&b.is_stable))
            .then_with(|| a.release_date.cmp(&b.release_date))
            .then_with(|| a.is_primary_slug.cmp(&b.is_primary_slug))
            .then_with(|| {
                a.entry
                    .raw_score
                    .partial_cmp(&b.entry.raw_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.entry.aa_slug.cmp(&a.entry.aa_slug))
    }

    let mut grouped: BTreeMap<String, Candidate> = BTreeMap::new();

    for raw in raw_models {
        let Some(raw_score) = raw
            .evaluations
            .as_ref()
            .and_then(|e| e.artificial_analysis_intelligence_index)
        else {
            continue;
        };
        if !raw_score.is_finite() || raw_score < 0.0 {
            continue;
        }
        let slug = raw.slug.trim();
        let name = raw.name.trim();
        if slug.is_empty() || name.is_empty() {
            continue;
        }

        let slug_tokens = canonicalize_tokens(tokenize_model_name(slug));
        if slug_tokens.is_empty() {
            continue;
        }
        let name_base_tokens = canonicalize_tokens(tokenize_model_name(name));
        let name_effort = parse_effort_from_name(name);
        let (base_tokens, slug_effort) = strip_aa_slug_effort(&slug_tokens, &name_base_tokens);
        let base_key = canonicalize_tokens(base_tokens).join("-");
        if base_key.is_empty() {
            continue;
        }

        let effective_effort = match (name_effort, slug_effort) {
            (Some(from_name), _) => from_name,
            (None, Some(from_slug)) => from_slug,
            (None, None) => EFFORT_DEFAULT,
        };

        let rounded_score = raw_score.round().clamp(0.0, 100.0) as i32;
        let is_stable = !is_preview_or_snapshot_slug(&slug_tokens);
        let is_primary_slug = slug_tokens.join("-") == base_key;
        let release_date = raw.release_date.clone().unwrap_or_default();

        let candidate = Candidate {
            entry: AaModelRatingEntry {
                canonical_key: base_key.clone(),
                aa_slug: slug.to_string(),
                aa_name: name.to_string(),
                aa_creator: raw
                    .model_creator
                    .as_ref()
                    .map(|c| c.name.trim().to_string())
                    .unwrap_or_default(),
                selected_effort: effective_effort.label.to_string(),
                raw_score,
                score: rounded_score,
                release_date: raw.release_date.clone(),
            },
            effort_rank: effective_effort.rank,
            is_stable,
            release_date,
            is_primary_slug,
        };

        let replace = match grouped.get(&base_key) {
            None => true,
            Some(current) => compare_candidates(&candidate, current).is_gt(),
        };
        if replace {
            grouped.insert(base_key, candidate);
        }
    }

    grouped.into_values().map(|c| c.entry).collect()
}

fn strip_one_modifier_suffix(tokens: &[String]) -> Option<Vec<String>> {
    let n = tokens.len();
    if n < 2 {
        return None;
    }

    if n >= 3 {
        let pair = format!("{}-{}", tokens[n - 2], tokens[n - 1]);
        if matches!(
            pair.as_str(),
            "build-fast"
                | "extra-high"
                | "extra-low"
                | "non-reasoning"
                | "no-reasoning"
                | "non-thinking"
                | "no-thinking"
                | "chat-latest"
                | "default-fallback"
                | "fast-latest"
        ) {
            return Some(tokens[..n - 2].to_vec());
        }
    }

    if let Some(date_len) = is_date_like_suffix(tokens)
        && n > date_len
    {
        return Some(tokens[..n - date_len].to_vec());
    }

    let last = tokens[n - 1].as_str();
    if PROTECTED_PRODUCT_TOKENS.contains(&last) {
        return None;
    }

    if matches!(
        last,
        "max"
            | "xhigh"
            | "high"
            | "medium"
            | "med"
            | "low"
            | "minimal"
            | "xlow"
            | "none"
            | "off"
            | "tiered"
            | "reasoning"
            | "thinking"
            | "think"
            | "adaptive"
            | "extended"
            | "default"
            | "latest"
            | "preview"
            | "beta"
            | "exp"
            | "experimental"
            | "highspeed"
            | "1m"
            | "2m"
            | "128k"
            | "256k"
            | "instruct"
            | "chat"
            | "customtools"
    ) {
        if n == 2 && matches!(last, "chat" | "instruct" | "thinking" | "reasoning") {
            return None;
        }
        return Some(tokens[..n - 1].to_vec());
    }

    None
}

/// Precomputed lookup index over consolidated AA ratings.
#[derive(Debug, Clone, Default)]
pub struct AaRatingIndex {
    primary: HashMap<String, usize>,
    secondary: HashMap<String, usize>,
    entries: Vec<AaModelRatingEntry>,
}

impl AaRatingIndex {
    pub fn build(entries: Vec<AaModelRatingEntry>) -> Self {
        let mut primary = HashMap::with_capacity(entries.len());
        for (idx, entry) in entries.iter().enumerate() {
            primary.insert(entry.canonical_key.clone(), idx);
        }

        let mut secondary: HashMap<String, usize> = HashMap::new();
        let mut ambiguous_secondary: HashSet<String> = HashSet::new();

        let mut register_secondary = |key: String, idx: usize| {
            if key.is_empty() || primary.contains_key(&key) || ambiguous_secondary.contains(&key) {
                return;
            }
            if let Some(&existing) = secondary.get(&key) {
                if existing != idx {
                    secondary.remove(&key);
                    ambiguous_secondary.insert(key);
                }
            } else {
                secondary.insert(key, idx);
            }
        };

        for (idx, entry) in entries.iter().enumerate() {
            let tokens: Vec<String> = entry
                .canonical_key
                .split('-')
                .map(ToString::to_string)
                .collect();

            let mut stripped = tokens.clone();
            let mut changed = false;
            while let Some(last) = stripped.last().map(String::as_str) {
                if matches!(
                    last,
                    "preview" | "beta" | "exp" | "experimental" | "instruct"
                ) {
                    stripped.pop();
                    changed = true;
                } else if let Some(dlen) = is_date_like_suffix(&stripped)
                    && stripped.len() > dlen
                {
                    stripped.truncate(stripped.len() - dlen);
                    changed = true;
                } else {
                    break;
                }
            }
            if changed && stripped.len() >= 2 {
                register_secondary(stripped.join("-"), idx);
            }

            if tokens.len() >= 3 && OPTIONAL_VENDOR_PREFIXES.contains(&tokens[0].as_str()) {
                let without_vendor = tokens[1..].join("-");
                register_secondary(without_vendor, idx);
            }

            let name_key = normalize_model_key(&entry.aa_name);
            if name_key != entry.canonical_key {
                register_secondary(name_key, idx);
            }
        }

        Self {
            primary,
            secondary,
            entries,
        }
    }

    pub fn entries(&self) -> &[AaModelRatingEntry] {
        &self.entries
    }

    pub fn get_by_key(&self, canonical_key: &str) -> Option<&AaModelRatingEntry> {
        self.primary
            .get(canonical_key)
            .and_then(|&idx| self.entries.get(idx))
    }

    pub fn match_model(&self, model: &str) -> Option<&AaModelRatingEntry> {
        let base_tokens = canonicalize_tokens(tokenize_model_name(model));
        if base_tokens.is_empty() {
            return None;
        }

        if let Some(found) = self.match_token_sequence(&base_tokens) {
            return Some(found);
        }

        if base_tokens.len() >= 3 && OPTIONAL_VENDOR_PREFIXES.contains(&base_tokens[0].as_str()) {
            let trimmed = canonicalize_tokens(base_tokens[1..].to_vec());
            if let Some(found) = self.match_token_sequence(&trimmed) {
                return Some(found);
            }
        }

        None
    }

    fn match_token_sequence(&self, initial: &[String]) -> Option<&AaModelRatingEntry> {
        let mut current = initial.to_vec();
        for _ in 0..8 {
            let key = current.join("-");
            if let Some(&idx) = self.primary.get(&key) {
                return self.entries.get(idx);
            }
            if let Some(&idx) = self.secondary.get(&key) {
                return self.entries.get(idx);
            }
            let Some(next) = strip_one_modifier_suffix(&current) else {
                break;
            };
            let next = canonicalize_tokens(next);
            if next == current || next.is_empty() {
                break;
            }
            current = next;
        }
        None
    }
}

/// Resolve the effective rating for a concrete model name using the 3-tier hierarchy:
/// 1. Manual prefix rating (highest priority)
/// 2. Explicit user AA alias
/// 3. Automatic AA Intelligence Index match
pub fn resolve_effective_rating(
    model: &str,
    manual_entries: &[ModelRatingEntry],
    aa_aliases: &[AaModelAlias],
    aa_index: &AaRatingIndex,
    aa_synced_at: Option<&str>,
) -> Option<EffectiveModelRating> {
    if let Some(manual) = longest_matching_entry(model, manual_entries) {
        return Some(EffectiveModelRating {
            model_prefix: canonical_model_prefix(&manual.model_prefix),
            score: manual.score,
            raw_score: None,
            updated_at: manual.updated_at.clone(),
            source: EffectiveRatingSource::Manual,
            aa_slug: None,
            aa_name: None,
            selected_effort: None,
        });
    }
    let normalized = normalize_model_key(model);
    if !normalized.is_empty()
        && normalized != canonical_model_prefix(model)
        && let Some(manual) = longest_matching_entry(&normalized, manual_entries)
    {
        return Some(EffectiveModelRating {
            model_prefix: canonical_model_prefix(&manual.model_prefix),
            score: manual.score,
            raw_score: None,
            updated_at: manual.updated_at.clone(),
            source: EffectiveRatingSource::Manual,
            aa_slug: None,
            aa_name: None,
            selected_effort: None,
        });
    }

    if !aa_aliases.is_empty() {
        let alias_as_entries: Vec<ModelRatingEntry> = aa_aliases
            .iter()
            .map(|a| ModelRatingEntry {
                model_prefix: a.model_prefix.clone(),
                score: 0,
                updated_at: a.updated_at.clone(),
            })
            .collect();
        let matched_alias_prefix = longest_matching_entry(model, &alias_as_entries)
            .or_else(|| {
                (!normalized.is_empty())
                    .then(|| longest_matching_entry(&normalized, &alias_as_entries))
                    .flatten()
            })
            .map(|e| e.model_prefix.clone());

        if let Some(prefix) = matched_alias_prefix
            && let Some(alias) = aa_aliases.iter().find(|a| a.model_prefix == prefix)
            && let Some(aa_entry) = aa_index.get_by_key(&alias.aa_canonical_key)
        {
            return Some(EffectiveModelRating {
                model_prefix: canonical_model_prefix(&alias.model_prefix),
                score: aa_entry.score,
                raw_score: Some(aa_entry.raw_score),
                updated_at: aa_synced_at
                    .map(ToString::to_string)
                    .unwrap_or_else(|| alias.updated_at.clone()),
                source: EffectiveRatingSource::AaAlias,
                aa_slug: Some(aa_entry.aa_slug.clone()),
                aa_name: Some(aa_entry.aa_name.clone()),
                selected_effort: Some(aa_entry.selected_effort.clone()),
            });
        }
    }

    let aa_entry = aa_index.match_model(model)?;
    Some(EffectiveModelRating {
        model_prefix: aa_entry.canonical_key.clone(),
        score: aa_entry.score,
        raw_score: Some(aa_entry.raw_score),
        updated_at: aa_synced_at.unwrap_or("").to_string(),
        source: EffectiveRatingSource::AaAuto,
        aa_slug: Some(aa_entry.aa_slug.clone()),
        aa_name: Some(aa_entry.aa_name.clone()),
        selected_effort: Some(aa_entry.selected_effort.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(slug: &str, name: &str, score: Option<f64>, creator: &str) -> AaRawModel {
        AaRawModel {
            slug: slug.to_string(),
            name: name.to_string(),
            release_date: Some("2026-10-01".to_string()),
            model_creator: Some(AaRawCreator {
                name: creator.to_string(),
            }),
            evaluations: Some(AaRawEvaluations {
                artificial_analysis_intelligence_index: score,
            }),
        }
    }

    #[test]
    fn normalizes_separators_case_namespaces_and_claude_order() {
        for input in [
            "claude-opus-5-5",
            "claude-opus-5.5",
            "Claude-Opus-5.5",
            "Claude Opus 5.5",
            "claude_opus_5_5",
            "anthropic/claude-opus-5.5",
            "Claude Opus 5.5 (Max, Default Fallback)",
            "anthropic/claude-opus-5-5:free",
        ] {
            assert_eq!(normalize_model_key(input), "claude-opus-5-5", "{input}");
        }
        assert_eq!(
            normalize_model_key("claude-4-5-sonnet"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_key("claude-sonnet-4.5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_key("Claude 4.5 Sonnet"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model_key("claude-4-5-haiku"), "claude-haiku-4-5");
        assert_eq!(normalize_model_key("claude-4-1-opus"), "claude-opus-4-1");
        assert_eq!(normalize_model_key("claude-5-fable"), "claude-fable-5");
        assert_eq!(normalize_model_key("claude-fable-5"), "claude-fable-5");
        assert_eq!(normalize_model_key("claude-35-sonnet"), "claude-sonnet-3-5");
        assert_eq!(
            normalize_model_key("qwen3-5-397b-a17b"),
            "qwen-3-5-397b-a17b"
        );
        assert_eq!(
            normalize_model_key("Qwen3.5 397B A17B"),
            "qwen-3-5-397b-a17b"
        );
        assert_eq!(
            normalize_model_key("qwen-3.5-397b-a17b"),
            "qwen-3-5-397b-a17b"
        );
        assert_eq!(normalize_model_key("GPT-6.2 Astra"), "gpt-6-2-astra");
        assert_eq!(normalize_model_key("gpt-6.2-astra"), "gpt-6-2-astra");
        assert_eq!(normalize_model_key("gemini-3.8-flash"), "gemini-3-8-flash");
        assert_eq!(normalize_model_key("grok-4.7"), "grok-4-7");
    }

    #[test]
    fn consolidation_selects_highest_reasoning_effort_tier_strictly() {
        let models = vec![
            raw(
                "claude-opus-5-5-high",
                "Claude Opus 5.5 (High, Default Fallback)",
                Some(51.8),
                "Anthropic",
            ),
            raw(
                "claude-opus-5-5-xhigh",
                "Claude Opus 5.5 (Xhigh, Default Fallback)",
                Some(57.4),
                "Anthropic",
            ),
            raw(
                "claude-opus-5-5",
                "Claude Opus 5.5 (Max, Default Fallback)",
                Some(59.1),
                "Anthropic",
            ),
            raw(
                "claude-fable-5-1-xhigh",
                "Claude Fable 5.1 (Xhigh, Default Fallback)",
                Some(50.0),
                "Anthropic",
            ),
            raw(
                "claude-fable-5-1",
                "Claude Fable 5.1 (Max, Default Fallback)",
                Some(49.2),
                "Anthropic",
            ),
            raw(
                "gemini-4-carbon-low",
                "Gemini 4 Carbon (Low)",
                Some(54.2),
                "Google",
            ),
            raw(
                "gemini-4-carbon-medium",
                "Gemini 4 Carbon (Medium)",
                Some(58.3),
                "Google",
            ),
            raw(
                "gemini-4-carbon",
                "Gemini 4 Carbon (High)",
                Some(60.0),
                "Google",
            ),
            raw(
                "qwen3-7-max",
                "Qwen3.7 Max (Reasoning)",
                Some(46.2),
                "Alibaba",
            ),
            raw(
                "qwen3-7-max-non-reasoning",
                "Qwen3.7 Max (Non-reasoning)",
                Some(39.0),
                "Alibaba",
            ),
            raw("gpt-5-6-sol", "GPT-5.6 Sol", None, "OpenAI"),
        ];
        let index = AaRatingIndex::build(consolidate_aa_models(&models));
        let opus = index.get_by_key("claude-opus-5-5").unwrap();
        assert_eq!(opus.score, 59);
        assert_eq!(opus.raw_score, 59.1);
        assert_eq!(opus.selected_effort, "max");

        let fable = index.get_by_key("claude-fable-5-1").unwrap();
        assert_eq!(fable.score, 49);
        assert_eq!(fable.raw_score, 49.2);
        assert_eq!(fable.selected_effort, "max");

        let carbon = index.get_by_key("gemini-4-carbon").unwrap();
        assert_eq!(carbon.score, 60);
        assert_eq!(carbon.selected_effort, "high");

        let qwen_max = index.get_by_key("qwen-3-7-max").unwrap();
        assert_eq!(qwen_max.score, 46);
        assert_eq!(qwen_max.selected_effort, "reasoning");
        assert!(index.get_by_key("gpt-5-6-sol").is_none());
    }

    #[test]
    fn matches_all_vps_and_provider_model_variants_without_false_positives() {
        let models = vec![
            raw(
                "claude-fable-5-2",
                "Claude Fable 5.2 (Max)",
                Some(65.0),
                "Anthropic",
            ),
            raw("gpt-6-2-astra", "GPT-6.2 Astra (Max)", Some(62.0), "OpenAI"),
            raw(
                "gemini-4-neon",
                "Gemini 4 Neon (High)",
                Some(61.0),
                "Google",
            ),
            raw(
                "claude-opus-5-5",
                "Claude Opus 5.5 (Max)",
                Some(59.1),
                "Anthropic",
            ),
            raw("gpt-6-2-sol", "GPT-6.2 Sol (Max)", Some(57.0), "OpenAI"),
            raw("gpt-6-astra", "GPT-6 Astra (Max)", Some(56.9), "OpenAI"),
            raw(
                "claude-sonnet-5-5",
                "Claude Sonnet 5.5 (Max)",
                Some(54.5),
                "Anthropic",
            ),
            raw("gpt-6-1-sol", "GPT-6.1 Sol (Max)", Some(54.6), "OpenAI"),
            raw(
                "gemini-4-hydrogen",
                "Gemini 4 Hydrogen (High)",
                Some(54.0),
                "Google",
            ),
            raw("gpt-6-1-terra", "GPT-6.1 Terra (Max)", Some(51.3), "OpenAI"),
            raw(
                "gemini-4-argon",
                "Gemini 4 Argon (High)",
                Some(50.4),
                "Google",
            ),
            raw("gpt-6-sol", "GPT-6 Sol (Max)", Some(44.0), "OpenAI"),
            raw(
                "claude-haiku-5-5",
                "Claude Haiku 5.5 (Max)",
                Some(36.0),
                "Anthropic",
            ),
            raw("grok-4-7", "Grok 4.7 (Xhigh)", Some(35.0), "xAI"),
            raw(
                "gemini-3-8-flash",
                "Gemini 3.8 Flash (High)",
                Some(31.0),
                "Google",
            ),
            raw("gpt-6-luna", "GPT-6 Luna (Max)", Some(28.0), "OpenAI"),
            raw("gpt-5-6-luna", "GPT-5.6 Luna (Max)", Some(24.0), "OpenAI"),
            raw(
                "claude-4-5-sonnet",
                "Claude 4.5 Sonnet (Reasoning)",
                Some(48.0),
                "Anthropic",
            ),
            raw(
                "gemini-3-1-pro-preview",
                "Gemini 3.1 Pro Preview (High)",
                Some(52.0),
                "Google",
            ),
            raw("mistral-medium", "Mistral Medium", Some(33.0), "Mistral"),
            raw(
                "nvidia-nemotron-3-nano-30b-a3b",
                "NVIDIA Nemotron 3 Nano 30B A3B (Reasoning)",
                Some(30.0),
                "NVIDIA",
            ),
            raw(
                "minimax-m2-5",
                "MiniMax-M2.5 (Reasoning)",
                Some(42.0),
                "MiniMax",
            ),
        ];
        let index = AaRatingIndex::build(consolidate_aa_models(&models));
        let expected_hits = [
            ("claude-fable-5-2", 65),
            ("claude-fable-5.2", 65),
            ("Claude Fable 5.2", 65),
            ("gpt-6.2-astra", 62),
            ("GPT-6.2 Astra", 62),
            ("gemini-4-neon", 61),
            ("gemini-4-neon-low", 61),
            ("gemini-4-neon-xhigh", 61),
            ("claude-opus-5-5", 59),
            ("claude-opus-5.5", 59),
            ("Claude Opus 5.5", 59),
            ("claude-opus-5-5-low", 59),
            ("claude-opus-5-5-xhigh", 59),
            ("anthropic/claude-opus-5.5", 59),
            ("gpt-6.2-sol", 57),
            ("gpt-6-astra", 57),
            ("claude-sonnet-5-5", 55),
            ("claude-sonnet-5.5", 55),
            ("claude-sonnet-5-5-max", 55),
            ("gpt-6.1-sol", 55),
            ("gemini-4-hydrogen-medium", 54),
            ("gpt-6.1-terra", 51),
            ("gemini-4-argon-high", 50),
            ("gpt-6-sol", 44),
            ("claude-haiku-5-5-medium", 36),
            ("grok-4.7", 35),
            ("grok-4.7-build-fast", 35),
            ("gemini-3.8-flash-tiered", 31),
            ("gemini-3.8-flash-high", 31),
            ("gpt-6-luna", 28),
            ("gpt-5.6-luna", 24),
            ("claude-sonnet-4-5-20250929", 48),
            ("anthropic/claude-sonnet-4.5", 48),
            ("gemini-3-1-pro", 52),
            ("gemini-3-1-pro-high", 52),
            ("mistral-medium", 33),
            ("nemotron-3-nano-30b-a3b", 30),
            ("MiniMax-M2.5-highspeed", 42),
        ];
        for (model, score) in expected_hits {
            let matched = index
                .match_model(model)
                .unwrap_or_else(|| panic!("expected {model} to match AA"));
            assert_eq!(matched.score, score, "score mismatch for {model}");
        }
        for unmatched in [
            "codex-auto-review",
            "gpt-5.6-sol",
            "gpt-6-2-mini",
            "gpt-6-astra-mini",
            "gemini-3.8-flash-lite",
            "claude-opus-5-55",
            "claude-opus-6",
            "grok-4.7-mini",
            "deepseek-chat",
            "",
        ] {
            assert!(
                index.match_model(unmatched).is_none(),
                "expected {unmatched} to remain unmatched, got {:?}",
                index.match_model(unmatched)
            );
        }
    }

    #[test]
    fn effective_resolution_prefers_manual_then_alias_then_aa_auto() {
        let index = AaRatingIndex::build(consolidate_aa_models(&[
            raw(
                "claude-opus-5-5",
                "Claude Opus 5.5 (Max)",
                Some(59.1),
                "Anthropic",
            ),
            raw("gpt-6-2-astra", "GPT-6.2 Astra (Max)", Some(62.0), "OpenAI"),
        ]));
        let manual = vec![ModelRatingEntry {
            model_prefix: "claude-opus-5-5".to_string(),
            score: 77,
            updated_at: "2026-11-20T00:00:00.000Z".to_string(),
        }];
        let aliases = vec![AaModelAlias {
            model_prefix: "my-custom-astra".to_string(),
            aa_canonical_key: "gpt-6-2-astra".to_string(),
            updated_at: "2026-11-20T00:00:00.000Z".to_string(),
        }];

        let r1 = resolve_effective_rating(
            "Claude Opus 5.5",
            &manual,
            &aliases,
            &index,
            Some("2026-11-23T00:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(r1.source, EffectiveRatingSource::Manual);
        assert_eq!(r1.score, 77);

        let r2 = resolve_effective_rating(
            "my-custom-astra-v2",
            &manual,
            &aliases,
            &index,
            Some("2026-11-23T00:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(r2.source, EffectiveRatingSource::AaAlias);
        assert_eq!(r2.score, 62);
        assert_eq!(r2.aa_slug.as_deref(), Some("gpt-6-2-astra"));

        let r3 = resolve_effective_rating(
            "gpt-6.2-astra",
            &manual,
            &aliases,
            &index,
            Some("2026-11-23T00:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(r3.source, EffectiveRatingSource::AaAuto);
        assert_eq!(r3.score, 62);
        assert_eq!(r3.raw_score, Some(62.0));
    }
}
