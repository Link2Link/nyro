//! Suggest a Sub2API key's group platform.
//!
//! Sub2API exposes no API-key endpoint that names the group platform, so the
//! suggestion is inferred from evidence the key can read without spending
//! tokens:
//!
//! - `GET /v1/models` shape: the OpenAI platform answers in OpenAI's format
//!   (`object` + `owned_by`), Grok adds reasoning-effort fields
//!   (`owned_by: "xai"`); every other platform uses the Claude-style shape;
//! - model families in that list (`claude-*`, `gemini-*`, `deepseek-*`, ...);
//! - whether `GET /v1beta/models` is accepted (Gemini groups, and Composite
//!   groups that serve Gemini models) or rejected with 400;
//! - the group name from `GET /v1/sub2api/billing?include=group`, as a
//!   tie-breaker only.
//!
//! Groups without model mappings list Sub2API's static defaults (Claude's for
//! most platforms), so a pure Claude list stays a medium-confidence guess.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Sub2ApiPlatform;
use super::url::sub2api_root;

const DETECT_TIMEOUT: Duration = Duration::from_secs(15);
const SAMPLE_MODELS: usize = 12;

/// How sure the suggestion is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// Admin input: the address and key typed into the form.
#[derive(Debug, Clone, Deserialize)]
pub struct Sub2ApiDetectInput {
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub use_proxy: bool,
}

/// Suggestion returned to the WebUI. `platform` is `None` when the evidence
/// fits several platforms; `candidates` then lists them.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sub2ApiPlatformDetection {
    pub platform: Option<String>,
    pub confidence: Confidence,
    pub candidates: Vec<String>,
    pub group_name: Option<String>,
    pub model_count: usize,
    pub sample_models: Vec<String>,
    /// `/v1beta/models` accepted (`true`), rejected (`false`) or unknown.
    pub gemini_route: Option<bool>,
    /// English, human-readable reasons.
    pub evidence: Vec<String>,
}

/// Shape of the `/v1/models` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListShape {
    OpenAI,
    Grok,
    Generic,
}

/// Parsed `/v1/models` listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelListing {
    pub shape: ListShape,
    pub ids: Vec<String>,
}

impl ModelListing {
    pub fn parse(body: &Value) -> Option<Self> {
        let data = body.get("data")?.as_array()?;
        let grok = data.iter().any(|item| {
            item.get("owned_by").and_then(Value::as_str) == Some("xai")
                || item.get("reasoningEfforts").is_some()
                || item.get("supportsReasoningEffort").is_some()
        });
        let openai = !data.is_empty()
            && data.iter().all(|item| {
                item.get("object").and_then(Value::as_str) == Some("model")
                    && item.get("owned_by").and_then(Value::as_str) == Some("openai")
            });
        let shape = if grok {
            ListShape::Grok
        } else if openai {
            ListShape::OpenAI
        } else {
            ListShape::Generic
        };
        let ids = data
            .iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        Some(Self { shape, ids })
    }
}

/// Model family of one id, as far as platform inference cares.
fn family(id: &str) -> &'static str {
    let id = id.trim().to_ascii_lowercase();
    let id = id.rsplit('/').next().unwrap_or(&id).to_string();
    if id.starts_with("claude") {
        "claude"
    } else if id.starts_with("gemini") || id.starts_with("gemma") || id.starts_with("tab_") {
        "gemini"
    } else if id.starts_with("gpt-oss") {
        // Antigravity serves gpt-oss next to Claude and Gemini.
        "gpt-oss"
    } else if id.starts_with("grok") {
        "grok"
    } else if id.starts_with("deepseek") {
        "deepseek"
    } else if id.starts_with("kimi") || id.starts_with("moonshot") {
        "kimi"
    } else if id.starts_with("glm") || id.starts_with("chatglm") {
        "glm"
    } else if id.starts_with("minimax") || id.starts_with("abab") {
        "minimax"
    } else if id.starts_with("gpt")
        || id.starts_with("chatgpt")
        || id.starts_with("codex")
        || id.starts_with("o1")
        || id.starts_with("o3")
        || id.starts_with("o4")
    {
        "openai"
    } else {
        "other"
    }
}

/// Platform a group name points at (keyword match), if any.
fn platform_from_group_name(name: &str) -> Option<Sub2ApiPlatform> {
    let name = name.to_lowercase();
    let table: &[(&[&str], Sub2ApiPlatform)] = &[
        (&["antigravity", "反重力"], Sub2ApiPlatform::Antigravity),
        (&["composite", "组合"], Sub2ApiPlatform::Composite),
        (&["opencode"], Sub2ApiPlatform::OpenCodeGo),
        (
            &["command code", "commandcode"],
            Sub2ApiPlatform::CommandCode,
        ),
        (&["cline"], Sub2ApiPlatform::Cline),
        (&["gemini"], Sub2ApiPlatform::Gemini),
        (&["grok", "xai"], Sub2ApiPlatform::Grok),
        (&["kimi", "moonshot", "月之暗面"], Sub2ApiPlatform::Kimi),
        (&["glm", "zhipu", "智谱"], Sub2ApiPlatform::Zhipu),
        (&["deepseek", "深度求索"], Sub2ApiPlatform::DeepSeek),
        (&["minimax"], Sub2ApiPlatform::MiniMax),
        (
            &["openai", "codex", "gpt", "chatgpt"],
            Sub2ApiPlatform::OpenAI,
        ),
        (&["claude", "anthropic"], Sub2ApiPlatform::Anthropic),
    ];
    table
        .iter()
        .find(|(keywords, _)| keywords.iter().any(|keyword| name.contains(keyword)))
        .map(|(_, platform)| *platform)
}

const AGGREGATORS: [Sub2ApiPlatform; 4] = [
    Sub2ApiPlatform::Composite,
    Sub2ApiPlatform::OpenCodeGo,
    Sub2ApiPlatform::CommandCode,
    Sub2ApiPlatform::Cline,
];

/// Infer the platform from collected evidence (pure; see module docs).
pub fn infer(
    listing: &ModelListing,
    gemini_route: Option<bool>,
    group_name: Option<&str>,
) -> Sub2ApiPlatformDetection {
    use Confidence::*;
    use Sub2ApiPlatform as P;

    let families: BTreeSet<&str> = listing.ids.iter().map(|id| family(id)).collect();
    let mut evidence = Vec::new();
    let mut candidates: Vec<P> = Vec::new();
    let only =
        |allowed: &[&str]| !families.is_empty() && families.iter().all(|f| allowed.contains(f));

    let (platform, confidence) = match listing.shape {
        ListShape::Grok => {
            evidence.push("The model list uses Sub2API's Grok format.".to_string());
            (Some(P::Grok), High)
        }
        ListShape::OpenAI => {
            evidence.push("The model list uses Sub2API's OpenAI format.".to_string());
            (Some(P::OpenAI), High)
        }
        ListShape::Generic if gemini_route == Some(true) => {
            evidence.push("The key is accepted on /v1beta (Gemini routes).".to_string());
            if families.is_empty() || only(&["gemini"]) {
                (Some(P::Gemini), High)
            } else {
                evidence.push(
                    "Models from several families are listed next to the Gemini route.".to_string(),
                );
                candidates = vec![P::Composite, P::Gemini];
                (Some(P::Composite), Medium)
            }
        }
        ListShape::Generic => {
            if gemini_route == Some(false) {
                evidence.push("The key is rejected on /v1beta (not a Gemini group).".to_string());
            }
            let gemini_rejected_or_unknown = if gemini_route == Some(false) {
                High
            } else {
                Medium
            };
            if families.is_empty() {
                evidence.push("The group lists no models.".to_string());
                (None, Low)
            } else if only(&["claude", "gemini", "gpt-oss"])
                && families.contains("gemini")
                && (families.contains("claude") || families.contains("gpt-oss"))
            {
                evidence.push(
                    "Claude and Gemini models are listed together, as Antigravity groups do."
                        .to_string(),
                );
                (Some(P::Antigravity), gemini_rejected_or_unknown)
            } else if only(&["gemini"]) && gemini_route.is_none() {
                candidates = vec![P::Gemini, P::Antigravity];
                evidence.push(
                    "Only Gemini models are listed; the Gemini route could not be verified.".into(),
                );
                (None, Low)
            } else if only(&["gemini"]) {
                evidence.push(
                    "Only Gemini models are listed, but /v1beta is not accepted.".to_string(),
                );
                candidates = vec![P::Antigravity, P::Gemini];
                (Some(P::Antigravity), Medium)
            } else if only(&["claude"]) {
                evidence.push(
                    "Only Claude models are listed. Groups without model mappings also list \
                     Claude defaults, so confirm the platform in Sub2API."
                        .to_string(),
                );
                candidates = vec![P::Anthropic];
                (Some(P::Anthropic), Medium)
            } else if let Some(platform) = [
                ("deepseek", P::DeepSeek),
                ("kimi", P::Kimi),
                ("glm", P::Zhipu),
                ("minimax", P::MiniMax),
            ]
            .into_iter()
            .find(|(family, _)| only(&[family]))
            .map(|(_, platform)| platform)
            {
                evidence.push(format!(
                    "Only {} models are listed.",
                    match platform {
                        P::DeepSeek => "DeepSeek",
                        P::Kimi => "Kimi",
                        P::Zhipu => "GLM",
                        _ => "MiniMax",
                    }
                ));
                (Some(platform), High)
            } else {
                evidence.push(
                    "Models from several vendors are listed, as aggregator and composite groups do."
                        .to_string(),
                );
                candidates = AGGREGATORS.to_vec();
                (None, Low)
            }
        }
    };

    // The group name only settles ambiguity; it never overrides strong evidence.
    let named = group_name.and_then(platform_from_group_name);
    let (platform, confidence) = match (named, platform, confidence) {
        (Some(named), _, Low | Medium) if candidates.is_empty() || candidates.contains(&named) => {
            evidence.push(format!(
                "The group name suggests the {} platform.",
                named.id()
            ));
            (Some(named), Medium.max(confidence))
        }
        (Some(named), Some(current), _) if named != current => {
            evidence.push(format!(
                "The group name suggests {}, which the model evidence does not support.",
                named.id()
            ));
            (platform, confidence)
        }
        _ => (platform, confidence),
    };

    if let Some(platform) = platform
        && !candidates.contains(&platform)
        && !candidates.is_empty()
    {
        candidates.insert(0, platform);
    }
    Sub2ApiPlatformDetection {
        platform: platform.map(|platform| platform.id().to_string()),
        confidence,
        candidates: candidates
            .iter()
            .map(|platform| platform.id().to_string())
            .collect(),
        group_name: group_name.map(str::to_string),
        model_count: listing.ids.len(),
        sample_models: listing.ids.iter().take(SAMPLE_MODELS).cloned().collect(),
        gemini_route,
        evidence,
    }
}

async fn get(client: &reqwest::Client, url: &str, api_key: &str) -> anyhow::Result<(u16, String)> {
    let response = client
        .get(url)
        .bearer_auth(api_key.trim())
        .timeout(DETECT_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("request to {url} failed: {error}"))?;
    let status = response.status().as_u16();
    Ok((status, response.text().await.unwrap_or_default()))
}

/// Collect evidence from a Sub2API deployment and infer the platform.
pub async fn detect(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<Sub2ApiPlatformDetection> {
    let root = sub2api_root(base_url);
    anyhow::ensure!(!root.is_empty(), "enter the Sub2API address first");
    anyhow::ensure!(
        !api_key.trim().is_empty(),
        "enter the Sub2API API key first"
    );
    let url = reqwest::Url::parse(root).map_err(|_| anyhow::anyhow!("invalid Sub2API address"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "enter a Sub2API site root without credentials, query or fragment"
    );
    let models_url = format!("{root}/v1/models");
    let gemini_url = format!("{root}/v1beta/models");
    let billing_url = format!("{root}/v1/sub2api/billing?include=group");
    let (models, gemini, billing) = tokio::join!(
        get(client, &models_url, api_key),
        get(client, &gemini_url, api_key),
        get(client, &billing_url, api_key),
    );

    let (status, body) = models?;
    match status {
        200..=299 => {}
        401 => anyhow::bail!("the Sub2API API key is invalid or disabled (HTTP 401)"),
        _ => {
            let preview: String = body.chars().take(200).collect();
            anyhow::bail!("listing models failed (HTTP {status}): {preview}");
        }
    }
    let listing = serde_json::from_str::<Value>(&body)
        .ok()
        .as_ref()
        .and_then(ModelListing::parse)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the address did not answer like a Sub2API API (no model list); use the Sub2API site root"
            )
        })?;

    let gemini_route = match gemini {
        Ok((200..=299, body)) => serde_json::from_str::<Value>(&body)
            .ok()
            .map(|body| body.get("models").is_some_and(Value::is_array)),
        Ok((400 | 403 | 404, _)) => Some(false),
        _ => None,
    };
    let group_name = match billing {
        Ok((200..=299, body)) => serde_json::from_str::<Value>(&body).ok().and_then(|body| {
            body.get("group_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        }),
        _ => None,
    };
    Ok(infer(&listing, gemini_route, group_name.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generic(ids: &[&str]) -> ModelListing {
        ModelListing {
            shape: ListShape::Generic,
            ids: ids.iter().map(|id| id.to_string()).collect(),
        }
    }

    fn suggestion(detection: &Sub2ApiPlatformDetection) -> (Option<&str>, Confidence) {
        (detection.platform.as_deref(), detection.confidence)
    }

    #[test]
    fn listing_shapes_follow_sub2api_writers() {
        let openai = json!({"object":"list","data":[{"id":"gpt-5.6","object":"model","created":1,"owned_by":"openai","type":"model","display_name":"gpt-5.6"}]});
        let grok = json!({"object":"list","data":[{"id":"grok-4.7","object":"model","owned_by":"xai","reasoningEfforts":[{"value":"low"}]}]});
        let claude = json!({"object":"list","data":[{"id":"claude-sonnet-4-6","type":"model","display_name":"x","created_at":"2024-01-01T00:00:00Z"}]});
        assert_eq!(
            ModelListing::parse(&openai).unwrap().shape,
            ListShape::OpenAI
        );
        assert_eq!(ModelListing::parse(&grok).unwrap().shape, ListShape::Grok);
        let generic = ModelListing::parse(&claude).unwrap();
        assert_eq!(generic.shape, ListShape::Generic);
        assert_eq!(generic.ids, ["claude-sonnet-4-6"]);
        assert!(ModelListing::parse(&json!({"models": []})).is_none());
    }

    #[test]
    fn strong_shapes_and_routes_are_high_confidence() {
        let openai = ModelListing {
            shape: ListShape::OpenAI,
            ids: vec!["gpt-5.6".into()],
        };
        assert_eq!(
            suggestion(&infer(&openai, Some(false), None)),
            (Some("openai"), Confidence::High)
        );
        let grok = ModelListing {
            shape: ListShape::Grok,
            ids: vec!["grok-4.7".into()],
        };
        assert_eq!(
            suggestion(&infer(&grok, Some(false), None)),
            (Some("grok"), Confidence::High)
        );
        assert_eq!(
            suggestion(&infer(
                &generic(&["gemini-2.5-pro", "gemini-3.8-flash"]),
                Some(true),
                None
            )),
            (Some("gemini"), Confidence::High)
        );
        assert_eq!(
            suggestion(&infer(
                &generic(&[
                    "claude-sonnet-4-6",
                    "gemini-3.8-flash-high",
                    "gpt-oss-120b-medium"
                ]),
                Some(false),
                None
            )),
            (Some("antigravity"), Confidence::High)
        );
        for (ids, expected) in [
            (vec!["deepseek-chat", "deepseek-reasoner"], "deepseek"),
            (vec!["kimi-k3", "moonshot-v1-8k"], "kimi"),
            (vec!["glm-5.3", "glm-5.3-flash"], "zhipu"),
            (vec!["MiniMax-M3"], "minimax"),
        ] {
            assert_eq!(
                suggestion(&infer(&generic(&ids), Some(false), None)),
                (Some(expected), Confidence::High)
            );
        }
    }

    #[test]
    fn ambiguous_evidence_lowers_confidence_and_lists_candidates() {
        let unknown = infer(&generic(&["gemini-pro"]), None, None);
        assert_eq!(suggestion(&unknown), (None, Confidence::Low));
        assert!(unknown.candidates.contains(&"gemini".into()));
        let claude = infer(
            &generic(&["claude-opus-5-5", "claude-sonnet-5-5"]),
            Some(false),
            None,
        );
        assert_eq!(suggestion(&claude), (Some("anthropic"), Confidence::Medium));
        let mixed = infer(
            &generic(&["kimi-k3", "glm-5.3", "gpt-5.6"]),
            Some(false),
            None,
        );
        assert_eq!(suggestion(&mixed), (None, Confidence::Low));
        assert_eq!(
            mixed.candidates,
            ["composite", "opencode_go", "command_code", "cline"]
        );
        let composite = infer(
            &generic(&["claude-opus-5-5", "gemini-3.8-flash", "gpt-5.6"]),
            Some(true),
            None,
        );
        assert_eq!(
            suggestion(&composite),
            (Some("composite"), Confidence::Medium)
        );
        let empty = infer(&generic(&[]), None, None);
        assert_eq!(suggestion(&empty), (None, Confidence::Low));
    }

    #[test]
    fn group_names_break_ties_but_never_override_strong_evidence() {
        let mixed = infer(
            &generic(&["kimi-k3", "glm-5.3"]),
            Some(false),
            Some("OpenCode Go 套餐"),
        );
        assert_eq!(
            suggestion(&mixed),
            (Some("opencode_go"), Confidence::Medium)
        );
        let claude = infer(
            &generic(&["claude-opus-5-5"]),
            Some(false),
            Some("反重力 Claude"),
        );
        // Antigravity is not a candidate of a pure Claude list: evidence only.
        assert_eq!(suggestion(&claude).0, Some("anthropic"));
        let openai = ModelListing {
            shape: ListShape::OpenAI,
            ids: vec!["gpt-5.6".into()],
        };
        let strong = infer(&openai, Some(false), Some("Claude group"));
        assert_eq!(suggestion(&strong), (Some("openai"), Confidence::High));
        assert!(
            strong
                .evidence
                .iter()
                .any(|line| line.contains("anthropic"))
        );
        assert_eq!(strong.group_name.as_deref(), Some("Claude group"));
    }
}
