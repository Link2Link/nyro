//! Standard 7-step request/response pipeline shared by every
//! OpenAI-compatible vendor.
//!
//! # Usage
//!
//! Delegate `build_request` and `parse_response` to the free functions here:
//!
//! ```rust,ignore
//! use crate::provider::common::pipeline;
//!
//! async fn build_request(&self, req, ctx) -> Result<OutboundRequest> {
//!     pipeline::build_request(self, req, ctx).await
//! }
//! async fn parse_response(&self, resp, ctx) -> Result<AiResponse> {
//!     pipeline::parse_response(self, resp, ctx).await
//! }
//! ```

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::db::models::Provider;
use crate::error::GatewayError;
use crate::protocol::ids::{OPENAI_RESPONSES_V1, Protocol, ProtocolId};
use crate::provider::vendor::Vendor;

/// ChatGPT 消费级上游渠道（codex OAuth 直连 / sub2api 中转）。sub2api
/// 转发的仍是同一 chatgpt.com/backend-api/codex 消费级后端，出站体
/// 改写（采样参数剥离、reasoning 无状态回放防御、工具调用 ID 规范化、
/// Fast 模式注入）对两个渠道一致生效。
fn is_codex_consumer_channel(channel: &str) -> bool {
    channel.eq_ignore_ascii_case("codex") || channel.eq_ignore_ascii_case("sub2api")
}

/// OpenAI Responses 渠道开关「Fast 模式」：
///
/// 开启后，转发到上游的 OpenAI Responses 请求如果缺少 `service_tier` 字段，
/// 就补上 `"service_tier": "priority"`（对应 OpenAI 官方 Fast mode：
/// 优先处理、响应更快；默认值是 `auto`）。客户端显式携带的 `service_tier`
/// 永远优先，本函数不做覆盖。当前对 OpenAI 预设的 `sub2api` 与 `codex`
/// 渠道生效。
pub(crate) fn maybe_inject_openai_fast_mode(
    body: &mut Value,
    provider: &Provider,
    protocol: ProtocolId,
) {
    if protocol != OPENAI_RESPONSES_V1 {
        return;
    }
    maybe_inject_openai_fast_mode_for_protocol(
        body,
        provider.fast_mode,
        provider.channel.as_deref(),
        protocol.protocol,
    );
}

/// Apply the same Fast mode policy to requests that are built outside the
/// provider pipeline, such as the admin model probe.
pub(crate) fn maybe_inject_openai_fast_mode_for_protocol(
    body: &mut Value,
    fast_mode: bool,
    channel: Option<&str>,
    protocol: Protocol,
) {
    let is_openai_fast = fast_mode && channel.is_some_and(is_codex_consumer_channel);
    if !is_openai_fast || protocol != Protocol::OpenAIResponses {
        return;
    }
    if let Some(object) = body.as_object_mut()
        && !object.contains_key("service_tier")
    {
        object.insert(
            "service_tier".to_string(),
            Value::String("priority".to_string()),
        );
    }
}

/// Codex 消费级上游（`chatgpt.com/backend-api/codex`，含 sub2api 等转发同一
/// 后端的中转渠道）不接受 OpenAI Responses
/// 的 `max_output_tokens`/`temperature`/`top_p` 参数（codex-rs 协议契约里
/// 没有这些字段，OpenAI 自己的客户端不发它们）。原生直通与 IR 转码两条路径
/// 转发前都需要剥离，否则上游返回 400 "Unsupported parameter: max_output_tokens"。
pub(crate) fn maybe_sanitize_codex_consumer_request(body: &mut Value, provider: &Provider) {
    let is_codex = provider
        .channel
        .as_deref()
        .is_some_and(|channel| is_codex_consumer_channel(channel));
    if !is_codex {
        return;
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
        object.remove("temperature");
        object.remove("top_p");
    }
}

/// Codex 消费级上游（`chatgpt.com/backend-api/codex`，含 sub2api 中转）对
/// Responses 的
/// `reasoning` 输入项执行两层防御：
///
/// 1. **schema 校验**：`content` 数组最大长度为 0（合法载体是 `summary`
///    与 `encrypted_content`）。回放历史携带裸思维链文本会返回 400
///    `array_above_max_length` -> 剥掉非空 `content`。
/// 2. **无状态回放契约**：`store=false` 时服务端不持久化任何项，回放的
///    reasoning 项必须携带**本后端可解密**的 `encrypted_content` 才能重建。
///    多后端路由把会话中途切到其他 provider 再切回来时，外来 reasoning 项
///    既无法按 ID 重建（404 `Item with id ... not found`）、过不了 schema
///    校验（400 `array_above_max_length`）、也过不了密文校验 -> 整项剔除
///    （只损失该轮思维链上下文，请求可通过）。客户端显式 `store=true`
///    （依赖服务端状态）时不剔除。
///
///    外来项判据（任一命中即中转铸造，密文本后端必不可解密）：
///    - `encrypted_content` 为 null / 空白（中转不透出密文）；
///    - `id` 非原生形态。codex 消费级后端铸造的 reasoning ID 是 `rs_` +
///      64 位十六进制；中转方言（时间戳形态 `rs_resp_2026...`、UUID 形态
///      `rs_a44b4856-06b6-...`）铸造的密文用中转自己的密钥，回放即 400
///      `invalid_encrypted_content`（线上请求 06fbe5e2，2026-08-27：会话
///      中途切到 codex 直连后，16 项 null-密文外来项已被剔除，但 13 项带
///      密文的 UUID 形态项漏过旧判据，首个即被上游拒收）。ID 缺失的项
///      不在此判据内，维持原有保留行为。
///
/// 原生直通与 IR 转码两条路径转发前都需要执行；非 reasoning 项永不剔除。
///
/// codex 消费级后端铸造的 reasoning ID 形态：`rs_` + 64 位十六进制。
/// 其余形态（时间戳 `rs_resp_2026...`、UUID `rs_<uuid>` 等）皆外来方言。
fn is_native_codex_reasoning_id(id: &str) -> bool {
    match id.strip_prefix("rs_") {
        Some(hex) => hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

pub(crate) fn sanitize_codex_reasoning_content(body: &mut Value, provider: &Provider) {
    let is_codex = provider
        .channel
        .as_deref()
        .is_some_and(|channel| is_codex_consumer_channel(channel));
    if !is_codex {
        return;
    }
    let stateless = matches!(body.get("store"), Some(Value::Bool(false)));
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    if stateless {
        items.retain(|item| {
            let is_reasoning = item
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.eq_ignore_ascii_case("reasoning"));
            if !is_reasoning {
                return true;
            }
            // 无 usable encrypted_content 的 reasoning 项在无状态下无法重建。
            let has_encrypted_content = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty());
            // 密文存在但 ID 非原生形态：中转铸造的密文本后端必不可解密
            // （400 invalid_encrypted_content），同样整项剔除。
            let foreign_mint = item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !is_native_codex_reasoning_id(id));
            has_encrypted_content && !foreign_mint
        });
    }
    for item in items {
        let is_reasoning = item
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("reasoning"));
        if !is_reasoning {
            continue;
        }
        let strip = match item.get("content") {
            Some(Value::Array(entries)) => !entries.is_empty(),
            Some(Value::Null) | None => false,
            // 非数组取值同样过不了上游的数组校验，一并剥掉。
            Some(_) => true,
        };
        if strip && let Some(object) = item.as_object_mut() {
            object.remove("content");
        }
    }
}

/// Codex 消费级上游（codex 直连与 sub2api 中转）对工具调用项的 `id` 有
/// 硬前缀契约：
/// `custom_tool_call.id` 必须以 `ctc` 开头（配对 output 为 `ctco_`），
/// `function_call.id` 必须以 `fc` 开头。多后端路由把会话中途切到中转再
/// 切回来时，中转轮返回的工具调用可能带它自己的 ID 体系（如
/// `fc_call_...`），回放给 codex 会触发 400
/// `invalid_value: Expected an ID that begins with 'ctc'`。
///
/// 修复方式是 ID 规范化（改写）而非剔项：工具调用链
/// `reasoning → tool_call → tool_output` 中剔掉 tool_call 会让 output
/// 成为孤儿（同样 400），且工具结果是硬信息不该丢。改写 `id` 不影响
/// 配对——output 项通过 `call_id`（客户端↔模型配对键，中转轮自洽）
/// 引用调用，不通过 `id`。`call_id` 与 output 的 id 保持原样。
pub(crate) fn sanitize_codex_tool_call_ids(body: &mut Value, provider: &Provider) {
    let is_codex = provider
        .channel
        .as_deref()
        .is_some_and(|channel| is_codex_consumer_channel(channel));
    if !is_codex {
        return;
    }
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let required_prefix = if kind.eq_ignore_ascii_case("custom_tool_call") {
            "ctc"
        } else if kind.eq_ignore_ascii_case("function_call") {
            "fc"
        } else {
            continue;
        };
        let id = item.get("id").and_then(Value::as_str).map(str::to_string);
        let Some(id) = id else {
            continue;
        };
        if !id.starts_with(required_prefix)
            && let Some(object) = item.as_object_mut()
        {
            object.insert(
                "id".to_string(),
                Value::String(format!("{required_prefix}_{id}")),
            );
        }
    }
}

/// 火山引擎 Ark coding 上游（ark.cn-beijing.volces.com/api/coding）。
/// provider 的 vendor 字段可能是 preset id（ark-coding）、volcengine 或
/// custom（用户手配）--base_url 是跨配置形态最稳定的信号。
fn is_volcengine_ark(provider: &Provider, vendor_id: &str) -> bool {
    vendor_id.eq_ignore_ascii_case("ark-coding")
        || vendor_id.eq_ignore_ascii_case("volcengine")
        || provider.base_url.contains("volces.com")
}

/// Ark 上游实测拒收 `reasoning_effort: none` 的模型（400
/// InvalidParameter）。按线上证据逐个登记；未登记的模型维持原有方言
/// 策略（normalize），避免误伤接受 none 的模型。
const ARK_NONE_REJECTING_MODELS: &[&str] = &["glm-5.3"];

fn is_ark_none_rejecting_model(body_model: &str) -> bool {
    ARK_NONE_REJECTING_MODELS
        .iter()
        .any(|model| body_model.eq_ignore_ascii_case(model))
}

/// 思考强制开启的模型（模型级登记，跨上游生效；约束来自模型本体而非
/// 某一上游方言）。按官方证据逐个登记，未登记的模型维持原有方言策略：
/// - glm-5.3：官方文档明确「GLM-5.3 会始终启用思考功能」，thinking.type
///   仅支持 enabled；reasoning_effort 枚举收窄为 low/high/max（默认
///   max），迁移提示对旧用法 disabled 明言「否则，请求将失败」并指引改
///   为 enabled + low（docs.bigmodel.cn/cn/guide/models/text/glm-5.3，
///   2026-08 抓取）。据此 off 意图钳制为最小合法档 low。
/// - glm-5.3-flash：官方文档明确 thinking.type 仅支持 enabled、不支持关
///   闭思考，推荐 reasoning_effort: max，文本参数与 GLM-5.3 保持一致
///   （docs.bigmodel.cn/cn/guide/models/vlm/glm-5.3-flash，2026-08 抓
///   取）。off 意图同样钳制为 low。前缀匹配并要求边界字符非字母数字，
///   带日期等短横线后缀的变体（glm-5.3-flash-xxxx）一并覆盖。
const THINKING_MANDATORY_MODELS: &[&str] = &["glm-5.3", "glm-5.3-flash"];

fn is_thinking_mandatory_model(body_model: &str) -> bool {
    let model = body_model.trim();
    THINKING_MANDATORY_MODELS.iter().any(|prefix| {
        model.len() >= prefix.len()
            && model[..prefix.len()].eq_ignore_ascii_case(prefix)
            && model[prefix.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

pub(crate) fn apply_vendor_effort_policy(body: &mut Value, provider: &Provider) {
    let vendor_id = provider
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or_default();
    // grok 判定优先看请求体里的实际模型名：中继型 provider（如 sub2api、
    // 国模组合）的 vendor 字段是 custom，但转发的模型仍以 grok- 开头。
    // 模型名是跨中继最稳定的信号，比 vendor 字段可靠。
    let body_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_grok = vendor_id.eq_ignore_ascii_case("xai")
        || body_model.trim().to_ascii_lowercase().starts_with("grok-");
    if is_grok {
        super::effort_policy::drop_grok_effort(body);
    } else if body_model.trim().eq_ignore_ascii_case("gpt-6-astra") {
        // Live Codex rejection, request bcd2a3ed-af4b-44d7-8b94-3c7c29876396:
        // none is unsupported; low/medium/high/xhigh/max are accepted. Match
        // only the verified upstream model, including custom relay vendors.
        // Do not use the GLM clamp: it would narrow valid medium/xhigh tiers.
        super::effort_policy::clamp_off_effort_to_low(body);
    } else if is_thinking_mandatory_model(body_model) {
        // 模型本体不支持关闭思考（如 glm-5.3-flash）：off 意图钳为 low。
        // 必须排在 normalize 之前：misspelling disable 会先被归一成
        // none——对这类模型恰好是致死值。
        super::effort_policy::clamp_thinking_mandatory_effort(body);
    } else if vendor_id.eq_ignore_ascii_case("opencode-go") {
        // OpenCode zen：思考型模型连 none 都拒（400 [1210]），off 钳制为 low。
        super::effort_policy::clamp_opencode_effort(body);
    } else if is_volcengine_ark(provider, vendor_id) && is_ark_none_rejecting_model(body_model) {
        // 火山引擎 Ark glm-5.3 拒收 none（400 InvalidParameter，请求
        // 58e799fa）：off 意图钳制为 low。必须排在 normalize 之前，否则
        // misspelling disable 会先被归一成 none--恰好是致死值。
        super::effort_policy::clamp_volcengine_ark_effort(body);
    } else if !vendor_id.is_empty() {
        super::effort_policy::normalize_enum_effort(body);
    }
}

/// 模型映射级「max推理」覆盖（models.force_max_reasoning）：把 IR 的推理
/// 指令改写为最大档。无论客户端显式给了何种档位、预算制，还是完全未携带
/// 推理指令，都统一为 max（会话决策：①②③ 全覆盖）。`display`/summary
/// 偏好保留。只在转码 / compat 重编码路径调用；原生直通路径走 wire 级
/// [`apply_force_max_reasoning_body`]，两者不可混用。
pub(crate) fn force_max_reasoning_ir(req: &mut crate::protocol::ir::AiRequest) {
    req.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::Max);
    req.reasoning.enabled = true;
    req.reasoning.budget_tokens = None;
}

/// 直通路径的 wire 级 max 覆盖：直接改写出站 body，保持原生直通的逐字
/// 保真度（IR 级改写会触发 RequestMutated 语义、杀死 codex 类通道依赖的
/// 直通）。各协议表达与 IR 编码器一致：
/// - chat：顶层 `reasoning_effort: "max"`；
/// - Responses：嵌套 `reasoning.effort`（保留 summary 等兄弟键）；
/// - Anthropic：`thinking.type=adaptive` + `output_config.effort=max`
///   （镜像 [`crate::protocol::codec::anthropic::messages`] 编码器的
///   effort 表达）；
/// - Gemini：`thinkingConfig.thinkingLevel=high`（镜像
///   `google_thinking_level(Max)`：Gemini 无 max 档，顶格即 high）。
/// 调用方必须让 vendor effort 策略（grok max→xhigh 等）随后照常运行。
pub(crate) fn apply_force_max_reasoning_body(
    body: &mut Value,
    protocol: crate::protocol::ids::Protocol,
) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    match protocol {
        crate::protocol::ids::Protocol::OpenAICompatible => {
            object.insert(
                "reasoning_effort".to_string(),
                Value::String("max".to_string()),
            );
        }
        crate::protocol::ids::Protocol::OpenAIResponses => {
            let reasoning = object
                .entry("reasoning")
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(reasoning_obj) = reasoning.as_object_mut() {
                reasoning_obj.insert("effort".to_string(), Value::String("max".to_string()));
            }
        }
        crate::protocol::ids::Protocol::AnthropicMessages => {
            object.insert(
                "thinking".to_string(),
                serde_json::json!({ "type": "adaptive" }),
            );
            let output_config = object
                .entry("output_config")
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(config_obj) = output_config.as_object_mut() {
                config_obj.insert("effort".to_string(), Value::String("max".to_string()));
            }
        }
        crate::protocol::ids::Protocol::GoogleGemini => {
            let generation_config = object
                .entry("generationConfig")
                .or_insert_with(|| Value::Object(Default::default()));
            let thinking_config = generation_config.as_object_mut().map(|config| {
                config
                    .entry("thinkingConfig")
                    .or_insert_with(|| Value::Object(Default::default()))
            });
            if let Some(thinking_obj) = thinking_config.and_then(Value::as_object_mut) {
                thinking_obj.insert(
                    "thinkingLevel".to_string(),
                    Value::String("high".to_string()),
                );
            }
        }
    }
}

/// Standard `build_request` pipeline:
/// `pre_request → normalize_tool_results → pre_encode → codec_encode →
///  post_encode → auth_headers → build_url`.
pub async fn build_request<V>(
    vendor: &V,
    req: &mut crate::protocol::ir::AiRequest,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::provider::outbound::OutboundRequest, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    req.model = ctx.actual_model.to_string();

    let vendor_ctx = ctx.to_vendor_ctx();

    // 1. pre_request hook
    vendor
        .pre_request(&vendor_ctx, req, ctx.gw)
        .await
        .map_err(GatewayError::internal)?;

    // 2. normalize tool results
    crate::protocol::codec::tool_correlation::normalize_request_tool_results(req);

    // 3. pre_encode hook
    vendor
        .pre_encode(&vendor_ctx, req)
        .await
        .map_err(GatewayError::internal)?;

    // 4. codec encode
    let egress_handler = ctx.protocol.handler();
    let encoder = egress_handler.make_request_encoder();
    let (mut body, mut extra_headers) = encoder
        .encode_request(req)
        .map_err(GatewayError::internal)?;

    // 5. post_encode hook
    vendor
        .post_encode(&vendor_ctx, &mut body, &mut extra_headers)
        .await
        .map_err(GatewayError::internal)?;

    crate::provider::minimax::apply_chat_output_token_floor(
        &mut body,
        ctx.provider.vendor.as_deref(),
        ctx.protocol,
    );

    // 5b. sub2api Fast 模式：缺 service_tier 时补 priority（IR 转码路径）
    maybe_inject_openai_fast_mode(&mut body, ctx.provider, ctx.protocol);
    // 5c. Codex 消费级上游：剥离其拒绝的 Responses 参数（IR 转码路径）
    maybe_sanitize_codex_consumer_request(&mut body, ctx.provider);
    // 5d. Codex 消费级上游：剥离 reasoning 项的非空 content（IR 转码路径）
    sanitize_codex_reasoning_content(&mut body, ctx.provider);
    // 5e. Codex 消费级上游：规范化外来工具调用 ID 前缀（IR 转码路径）
    sanitize_codex_tool_call_ids(&mut body, ctx.provider);
    apply_vendor_effort_policy(&mut body, ctx.provider);

    // 6. auth headers
    //
    // OAuth drivers (codex, claude-code) stash their Bearer + provider-
    // specific headers in `RuntimeBinding.extra_headers` and ask the
    // dispatcher to skip the vendor's default `auth_headers` via
    // `ctx.disable_default_auth`. Skipping unconditionally would break
    // every API-key path; gating here keeps the OAuth invariant
    // ("no leaked empty x-api-key") in a single seam shared by every
    // openai-compatible adapter.
    let mut headers = if ctx.disable_default_auth {
        HeaderMap::new()
    } else {
        vendor.auth_headers(&vendor_ctx)
    };
    // Anthropic-protocol upstreams require `x-api-key` instead of
    // `Authorization: Bearer`. Most OpenAI-compatible vendors blindly emit
    // Bearer; rewrite here so any vendor with a declared anthropic endpoint
    // works out of the box.
    //
    // Skipped under `disable_default_auth`: when an OAuth driver owns auth
    // (claude-code uses `Bearer <oauth_token>` + `anthropic-beta=
    // oauth-2025-04-20`), `ctx.api_key` is the OAuth Bearer token, NOT a
    // real Anthropic API key. Rewriting it here would forward the Bearer
    // as a fake `x-api-key` and break the OAuth handshake.
    if !ctx.disable_default_auth
        && ctx.protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages
        && !headers.contains_key("x-api-key")
    {
        headers.remove(reqwest::header::AUTHORIZATION);
        if let Ok(v) = reqwest::header::HeaderValue::from_str(ctx.api_key) {
            headers.insert("x-api-key", v);
        }
    }
    headers.extend(extra_headers);

    // 7. build URL
    let egress_path = encoder.egress_path(ctx.actual_model, req.stream.enabled);
    let mut url = vendor.build_url(&vendor_ctx, ctx.egress_base_url, &egress_path);
    apply_explicit_auth_scheme(&mut headers, &mut url, ctx)?;

    Ok(crate::provider::outbound::OutboundRequest { url, headers, body })
}

/// Standard `parse_response` pipeline:
/// `pre_parse → codec_parse → reasoning_normalization → post_parse`.
pub async fn parse_response<V>(
    vendor: &V,
    resp: crate::provider::inbound::InboundResponse,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<crate::protocol::ir::AiResponse, GatewayError>
where
    V: crate::provider::vendor::Vendor,
{
    let vendor_ctx = ctx.to_vendor_ctx();
    let mut body = resp.body;

    // 1. pre_parse hook
    vendor
        .pre_parse(&vendor_ctx, &mut body)
        .await
        .map_err(GatewayError::internal)?;

    // 2. codec parse
    let egress_handler = ctx.protocol.handler();
    let parser = egress_handler.make_response_decoder();
    let mut ai_resp = parser
        .parse_response(body)
        .map_err(GatewayError::internal)?;

    // 3. reasoning normalization
    crate::protocol::codec::reasoning::normalize_response_reasoning(&mut ai_resp);

    // 4. post_parse hook
    vendor
        .post_parse(&vendor_ctx, &mut ai_resp)
        .await
        .map_err(GatewayError::internal)?;

    Ok(ai_resp)
}

/// PassThrough request builder: skips the IR codec entirely.
///
/// Used when [`crate::proxy::planner::ProtocolMode::Native`] is in effect
/// (ingress == egress) and the vendor declares no request mutations via
/// [`Vendor::declared_request_mutations`]. Authentication, URL/model
/// resolution, and narrowly scoped protocol defaults still apply; every other
/// client field is preserved. `is_stream` must come from the decoded ingress
/// request, not just a raw body field: native Gemini expresses streaming in the
/// URL action (`:streamGenerateContent`) rather than a JSON `stream` property.
fn normalize_openai_developer_roles(body: &mut serde_json::Value) {
    if let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for message in messages {
            if message.get("role").and_then(serde_json::Value::as_str) == Some("developer") {
                message["role"] = serde_json::Value::String("system".to_string());
            }
        }
    }
}

pub async fn passthrough_run(
    vendor: &dyn Vendor,
    mut raw_body: serde_json::Value,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
    is_stream: bool,
) -> Result<crate::provider::outbound::OutboundRequest, GatewayError> {
    let vendor_ctx = ctx.to_vendor_ctx();
    let is_openai_chat = ctx.protocol.protocol == crate::protocol::ids::Protocol::OpenAICompatible
        && ctx.protocol.name == "chat-completions";

    // Replace the model field with the route-configured actual model so the
    // upstream receives the real model name, not the client's virtual alias.
    if let Some(obj) = raw_body.as_object_mut() {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(ctx.actual_model.to_string()),
        );

        // OpenAI chat-completions streaming only populates `usage` in the final
        // chunk when the client opts in via `stream_options.include_usage`.
        // PassThrough bypasses `OpenAIEncoder`, which injects this on the
        // transcode path
        // (encoder.rs "Always include_usage when streaming"). Mirror it here so
        // usage stays observable for logging/cost on the native path too. An
        // explicit client `stream_options` is preserved verbatim (same
        // precedence as the encoder). Embeddings (non-streaming, different
        // shape), Responses, and Anthropic/Gemini (usage reported by default)
        // are excluded.
        if is_stream && is_openai_chat && !obj.contains_key("stream_options") {
            obj.insert(
                "stream_options".to_string(),
                serde_json::json!({"include_usage": true}),
            );
        }
    }

    // 模型映射级「max推理」覆盖（models.force_max_reasoning）：wire 级直改
    // 出站 body，保持原生直通保真（IR 改写会触发重编码、杀死 codex 类通
    // 道依赖的逐字直通）。随后的 vendor effort 策略照常裁决——max 是意图，
    // 出站档位仍受上游方言约束（grok max→xhigh 等）。
    if ctx.force_max_reasoning {
        apply_force_max_reasoning_body(&mut raw_body, ctx.protocol.protocol);
    }

    crate::provider::minimax::apply_chat_output_token_floor(
        &mut raw_body,
        ctx.provider.vendor.as_deref(),
        ctx.protocol,
    );

    if is_openai_chat {
        normalize_openai_developer_roles(&mut raw_body);
        apply_vendor_effort_policy(&mut raw_body, ctx.provider);
    }
    if ctx.protocol == crate::protocol::ids::OPENAI_RESPONSES_V1 {
        crate::protocol::codec::openai::responses::normalize_function_tool_defaults(&mut raw_body);
        // sub2api Fast 模式：缺 service_tier 时补 priority（Responses 直通路径）
        maybe_inject_openai_fast_mode(&mut raw_body, ctx.provider, ctx.protocol);
        // Codex 消费级上游：剥离其拒绝的 Responses 参数（Responses 直通路径）
        maybe_sanitize_codex_consumer_request(&mut raw_body, ctx.provider);
        // Codex 消费级上游：剥离 reasoning 项的非空 content（Responses 直通路径）
        sanitize_codex_reasoning_content(&mut raw_body, ctx.provider);
        // Codex 消费级上游：规范化外来工具调用 ID 前缀（Responses 直通路径）
        sanitize_codex_tool_call_ids(&mut raw_body, ctx.provider);
        // 供应商 effort 方言（Responses 直通路径）：grok 对 max 是 400 硬拒
        // （线上事故 65fffc9a），none/off 同样拒绝——此前只挂在 IR 转码与
        // Chat 透传两路，Responses 直通漏挂。
        apply_vendor_effort_policy(&mut raw_body, ctx.provider);
    }

    let mut headers = if ctx.disable_default_auth {
        HeaderMap::new()
    } else {
        vendor.auth_headers(&vendor_ctx)
    };

    // Anthropic-family egress: rewrite Bearer → x-api-key (mirrors build_request).
    if !ctx.disable_default_auth
        && ctx.protocol.protocol == crate::protocol::ids::Protocol::AnthropicMessages
        && !headers.contains_key("x-api-key")
    {
        headers.remove(reqwest::header::AUTHORIZATION);
        if let Ok(v) = reqwest::header::HeaderValue::from_str(ctx.api_key) {
            headers.insert("x-api-key", v);
        }
    }

    let egress_path = ctx
        .protocol
        .handler()
        .make_request_encoder()
        .egress_path(ctx.actual_model, is_stream);
    let mut url = vendor.build_url(&vendor_ctx, ctx.egress_base_url, &egress_path);
    apply_explicit_auth_scheme(&mut headers, &mut url, ctx)?;

    Ok(crate::provider::outbound::OutboundRequest {
        url,
        headers,
        body: raw_body,
    })
}

fn apply_explicit_auth_scheme(
    headers: &mut HeaderMap,
    url: &mut String,
    ctx: &crate::provider::vendor::ProviderCtx<'_>,
) -> Result<(), GatewayError> {
    let scheme = ctx.auth_scheme.trim();
    if scheme.is_empty() || scheme == "auto" {
        return Ok(());
    }

    headers.remove(reqwest::header::AUTHORIZATION);
    headers.remove("x-api-key");
    remove_query_api_key(url)?;

    match scheme {
        "bearer" => {
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))
                .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        "x-api-key" => {
            let value = reqwest::header::HeaderValue::from_str(ctx.api_key)
                .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
            headers.insert("x-api-key", value);
        }
        "query" => set_query_api_key(url, ctx.api_key)?,
        "none" => {}
        other => {
            return Err(GatewayError::internal(anyhow::anyhow!(
                "unsupported endpoint auth scheme: {other}"
            )));
        }
    }
    Ok(())
}

fn remove_query_api_key(raw_url: &mut String) -> Result<(), GatewayError> {
    rewrite_query_api_key(raw_url, None)
}

fn set_query_api_key(raw_url: &mut String, api_key: &str) -> Result<(), GatewayError> {
    rewrite_query_api_key(raw_url, Some(api_key))
}

fn rewrite_query_api_key(raw_url: &mut String, api_key: Option<&str>) -> Result<(), GatewayError> {
    let mut parsed = reqwest::Url::parse(raw_url)
        .map_err(|error| GatewayError::internal(anyhow::Error::new(error)))?;
    let existing = parsed
        .query_pairs()
        .filter(|(key, _)| key != "key")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    {
        let mut query = parsed.query_pairs_mut();
        query.clear();
        for (key, value) in existing {
            query.append_pair(&key, &value);
        }
        if let Some(api_key) = api_key {
            query.append_pair("key", api_key);
        }
    }
    *raw_url = parsed.to_string();
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests cover the `disable_default_auth` gate inside `build_request`.
    //! When `ProviderCtx.disable_default_auth` is set, the vendor's default
    //! `auth_headers` AND the Anthropic-egress `Authorization → x-api-key`
    //! rewrite MUST be suppressed. Both directions are pinned so a future
    //! refactor that flips a gate fails loudly.
    use super::*;
    use crate::Gateway;
    use crate::GatewayConfig;
    use crate::db::models::Provider;
    use crate::error::GatewayError;
    use crate::protocol::ids::{
        ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, ProtocolId,
    };
    use crate::protocol::ir::{AiRequest, AiResponse, ReasoningEffort};
    use crate::provider::inbound::InboundResponse;
    use crate::provider::outbound::OutboundRequest;
    use crate::provider::registry::VendorScope;
    use crate::provider::vendor::{ProviderCtx, Vendor};
    use crate::provider::vendor_ext::VendorCtx;
    use async_trait::async_trait;
    use reqwest::header::HeaderMap as ExtHeaderMap;
    use serde_json::Value;
    use uuid::Uuid;

    /// Stand-in vendor: injects `x-api-key: <ctx.api_key>`, mirroring
    /// how `AnthropicVendor::auth_headers` behaves.
    struct FakeApiKeyVendor;

    #[async_trait]
    impl Vendor for FakeApiKeyVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-test",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    "x-api-key",
                    reqwest::header::HeaderValue::from_str(ctx.api_key).unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-test"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-test", status, None)
        }
    }

    /// Emits `Authorization: Bearer <ctx.api_key>`, mirroring OpenAI-compat
    /// vendors. PR #105's rewrite turns this into `x-api-key` on Anthropic egress.
    struct FakeBearerVendor;

    #[async_trait]
    impl Vendor for FakeBearerVendor {
        fn scope(&self) -> VendorScope {
            VendorScope::Vendor {
                vendor_id: "fake-bearer",
            }
        }
        fn auth_headers(&self, ctx: &VendorCtx<'_>) -> ExtHeaderMap {
            let mut h = ExtHeaderMap::new();
            if !ctx.api_key.is_empty() {
                h.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", ctx.api_key))
                        .unwrap(),
                );
            }
            h
        }
        fn vendor_id(&self) -> &'static str {
            "fake-bearer"
        }
        fn supported_protocols(&self) -> &'static [ProtocolId] {
            &[OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1]
        }
        async fn build_request(
            &self,
            _req: &mut AiRequest,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<OutboundRequest, GatewayError> {
            unreachable!()
        }
        async fn parse_response(
            &self,
            _resp: InboundResponse,
            _ctx: &ProviderCtx<'_>,
        ) -> Result<AiResponse, GatewayError> {
            unreachable!()
        }
        fn map_error(&self, status: u16, _body: Value) -> GatewayError {
            GatewayError::upstream_status("fake-bearer", status, None)
        }
    }

    fn provider_with_api_key(api_key: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            vendor: Some("fake-test".into()),
            protocol: "openai".into(),
            base_url: "https://upstream.local".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("fake-test".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: api_key.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn minimal_chat_request() -> AiRequest {
        use crate::protocol::ir::{Message, MessageContent, Role};
        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            tool_calls: None,
            tool_call_id: None,
            meta: None,
        }];
        let mut req = AiRequest::new("ignored-by-actual-model", messages);
        req.meta.source_protocol = Some(OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1);
        req
    }

    async fn build_test_gateway() -> Gateway {
        let config = GatewayConfig {
            data_dir: std::env::temp_dir().join(format!("nyro-pipeline-test-{}", Uuid::new_v4())),
            ..Default::default()
        };
        let (gw, _log_rx) = Gateway::new(config).await.expect("gateway init");
        gw
    }

    #[tokio::test]
    async fn build_request_suppresses_default_auth_when_oauth_owns_it() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("would-leak-if-bypassed");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gpt-test",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth provider must not emit fallback x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
    }

    #[tokio::test]
    async fn build_request_keeps_default_auth_when_no_oauth() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gpt-test",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("apikey-abc"),
            "API-key path must still propagate x-api-key to upstream",
        );
    }

    // ── google/antigravity (Google AI Pro) channel ─────────────────────────────

    fn antigravity_provider() -> Provider {
        Provider {
            id: "p-antigravity".into(),
            name: "p-antigravity".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url: "https://cloudcode-pa.googleapis.com".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some("antigravity".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn google_default_provider(api_key: &str) -> Provider {
        Provider {
            id: "p-google".into(),
            name: "p-google".into(),
            vendor: Some("google".into()),
            protocol: "google-gemini".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: Vec::new(),
            preset_key: Some("google".into()),
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: api_key.into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
            last_test_success: None,
            last_test_at: None,
            is_enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn antigravity_credential() -> crate::auth::types::StoredCredential {
        crate::auth::types::StoredCredential {
            driver_key: "google".into(),
            scheme: "oauth_auth_code_pkce".into(),
            access_token: Some("ya29.access".into()),
            refresh_token: Some("1//refresh".into()),
            meta: serde_json::json!({
                "project_id": "cloudaicompanion-1",
                "email": "u@example.com",
            }),
            ..Default::default()
        }
    }

    fn antigravity_ctx<'a>(
        provider: &'a Provider,
        credential: Option<&'a crate::auth::types::StoredCredential>,
        gw: &'a Gateway,
    ) -> ProviderCtx<'a> {
        ProviderCtx {
            provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://cloudcode-pa.googleapis.com",
            api_key: "ya29.access",
            auth_scheme: "auto",
            actual_model: "gemini-2.5-pro",
            force_max_reasoning: false,
            credential,
            gw,
            disable_default_auth: true,
        }
    }

    #[tokio::test]
    async fn antigravity_channel_build_request_wraps_v1internal_envelope() {
        let gw = build_test_gateway().await;
        let provider = antigravity_provider();
        let credential = antigravity_credential();
        let mut req = minimal_chat_request();
        req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        let ctx = antigravity_ctx(&provider, Some(&credential), &gw);
        let out = build_request(&crate::provider::google::GoogleVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        // v1internal action path, no ?key= leak.
        assert_eq!(
            out.url,
            "https://cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
        // Envelope fields.
        assert_eq!(out.body["model"], "gemini-2.5-pro");
        assert_eq!(out.body["project"], "cloudaicompanion-1");
        assert_eq!(out.body["requestType"], "agent");
        assert_eq!(out.body["userAgent"], "antigravity");
        assert!(
            out.body["requestId"]
                .as_str()
                .unwrap()
                .starts_with("agent-")
        );
        // The standard Gemini body rides under request.contents.
        assert!(out.body["request"]["contents"].is_array());
    }

    #[tokio::test]
    async fn antigravity_channel_stream_uses_stream_action() {
        let gw = build_test_gateway().await;
        let provider = antigravity_provider();
        let credential = antigravity_credential();
        let mut req = minimal_chat_request();
        req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        req.stream.enabled = true;
        let ctx = antigravity_ctx(&provider, Some(&credential), &gw);
        let out = build_request(&crate::provider::google::GoogleVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.url,
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[tokio::test]
    async fn antigravity_channel_missing_project_id_fails_loudly() {
        let gw = build_test_gateway().await;
        let provider = antigravity_provider();
        let credential = crate::auth::types::StoredCredential {
            access_token: Some("ya29.access".into()),
            ..Default::default()
        };
        let mut req = minimal_chat_request();
        req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        let ctx = antigravity_ctx(&provider, Some(&credential), &gw);
        let error = build_request(&crate::provider::google::GoogleVendor, &mut req, &ctx)
            .await
            .expect_err("missing project_id must fail the build");
        assert!(
            error.to_string().contains("project_id"),
            "error should mention project_id: {error}"
        );
    }

    #[tokio::test]
    async fn antigravity_channel_parse_response_unwraps_envelope() {
        let gw = build_test_gateway().await;
        let provider = antigravity_provider();
        let credential = antigravity_credential();
        let mut req = minimal_chat_request();
        req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        let ctx = antigravity_ctx(&provider, Some(&credential), &gw);
        let body = serde_json::json!({
            "response": {
                "candidates": [{
                    "content": {"role": "model", "parts": [{"text": "hello there"}]},
                    "finishReason": "STOP",
                }],
                "usageMetadata": {
                    "promptTokenCount": 8,
                    "candidatesTokenCount": 5,
                },
            },
            "responseId": "resp-1",
        });
        let ai = parse_response(
            &crate::provider::google::GoogleVendor,
            InboundResponse { status: 200, body },
            &ctx,
        )
        .await
        .expect("parse_response succeeds");
        assert!(
            ai.content.contains("hello there"),
            "content: {:?}",
            ai.content
        );
        assert_eq!(ai.usage.completion_tokens, 5);
    }

    #[tokio::test]
    async fn google_default_channel_keeps_apikey_wire_contract() {
        let gw = build_test_gateway().await;
        let provider = google_default_provider("gem-key-1");
        let mut req = minimal_chat_request();
        req.meta.source_protocol = Some(GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA);
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://generativelanguage.googleapis.com",
            api_key: "gem-key-1",
            auth_scheme: "auto",
            actual_model: "gemini-2.5-flash",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&crate::provider::google::GoogleVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        // ?key= query auth on the standard path.
        assert!(
            out.url.starts_with(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
            ) && out.url.contains("key=gem-key-1"),
            "default channel URL: {}",
            out.url
        );
        // Body stays a plain Gemini request (no v1internal envelope).
        assert!(out.body.get("contents").is_some());
        assert!(out.body.get("request").is_none());
        // Mutation declarations stay passthrough for the default channel and
        // flip to IR only for the antigravity channel.
        let vendor = crate::provider::google::GoogleVendor;
        assert!(!vendor.declared_request_mutations_for(&provider));
        assert!(!vendor.declared_response_mutations_for(&provider));
        let antigravity = antigravity_provider();
        assert!(vendor.declared_request_mutations_for(&antigravity));
        assert!(vendor.declared_response_mutations_for(&antigravity));
    }

    #[tokio::test]
    async fn explicit_query_auth_uses_endpoint_credential_and_removes_header_auth() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("provider-level-key");
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://gemini.example",
            api_key: "endpoint-specific-key",
            auth_scheme: "query",
            actual_model: "gemini-test",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer stale"),
        );
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_static("stale"),
        );
        let mut url = "https://gemini.example/v1?existing=1&key=stale".to_string();

        apply_explicit_auth_scheme(&mut headers, &mut url, &ctx).unwrap();

        assert!(!headers.contains_key(reqwest::header::AUTHORIZATION));
        assert!(!headers.contains_key("x-api-key"));
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("existing").map(|value| value.as_ref()), Some("1"));
        assert_eq!(
            query.get("key").map(|value| value.as_ref()),
            Some("endpoint-specific-key")
        );
    }

    /// Pins the interaction: when an OAuth driver owns auth
    /// (`disable_default_auth=true`) AND the egress family is Anthropic, the
    /// `Authorization → x-api-key` rewrite must NOT fire.
    #[tokio::test]
    async fn build_request_does_not_rewrite_oauth_bearer_to_xapikey_on_anthropic_egress() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: "oauth_bearer_token_should_not_become_xapikey",
            auth_scheme: "auto",
            actual_model: "claude-sonnet-4-6",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: true,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert!(
            out.headers.get("x-api-key").is_none(),
            "OAuth Bearer must not be rewritten as x-api-key, got: {:?}",
            out.headers.get("x-api-key"),
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "default Authorization must be suppressed under disable_default_auth too, got: {:?}",
            out.headers.get(reqwest::header::AUTHORIZATION),
        );
    }

    /// Mirror of #105's main use case: API-key-mode OpenAI-compat vendor
    /// hitting Anthropic egress — the rewrite block MUST fire and turn
    /// `Authorization: Bearer` into `x-api-key`.
    #[tokio::test]
    async fn build_request_rewrites_bearer_to_xapikey_on_anthropic_egress_for_apikey_path() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("real-anthropic-key");
        let mut req = minimal_chat_request();
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://api.anthropic.com",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "claude-sonnet-4-6",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = build_request(&FakeBearerVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");
        assert_eq!(
            out.headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("real-anthropic-key"),
            "API-key path on Anthropic egress must produce x-api-key",
        );
        assert!(
            out.headers.get(reqwest::header::AUTHORIZATION).is_none(),
            "Authorization must be removed once x-api-key is set",
        );
    }

    #[tokio::test]
    async fn passthrough_native_gemini_stream_uses_stream_generate_content_path() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("gemini-key");
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://gemini-proxy.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gemini-2.5-flash",
            force_max_reasoning: false,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "contents": [{ "parts": [{ "text": "ping" }] }] }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.url,
            "https://gemini-proxy.local/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            "Gemini stream passthrough selects streaming from the URL action, not a body stream flag",
        );
    }

    fn openai_chat_ctx<'a>(
        provider: &'a Provider,
        gw: &'a Gateway,
        actual_model: &'a str,
    ) -> ProviderCtx<'a> {
        ProviderCtx {
            provider,
            protocol: OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model,
            force_max_reasoning: false,
            credential: None,
            gw,
            disable_default_auth: false,
        }
    }

    /// 模型映射开启「max推理」的 chat 上下文。
    fn openai_chat_ctx_forced<'a>(
        provider: &'a Provider,
        gw: &'a Gateway,
        actual_model: &'a str,
    ) -> ProviderCtx<'a> {
        ProviderCtx {
            force_max_reasoning: true,
            ..openai_chat_ctx(provider, gw, actual_model)
        }
    }

    /// The whole reason this injection exists: a native OpenAI chat-completions
    /// stream with no `stream_options` would otherwise be forwarded verbatim
    /// and the upstream would never report `usage`, so logging/cost sees 0/0.
    #[tokio::test]
    async fn passthrough_injects_include_usage_for_openai_streaming() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "messages": [{"role":"user","content":"ping"}], "stream": true }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["stream_options"]["include_usage"], true,
            "native openai stream without stream_options must get include_usage injected",
        );
    }

    #[tokio::test]
    async fn passthrough_converts_openai_developer_role_to_system() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "deepseek-v4-flash");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "messages": [
                    {"role": "developer", "content": "instructions"},
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"}
                ]
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(out.body["messages"][0]["role"], "system");
        assert_eq!(out.body["messages"][1]["role"], "user");
        assert_eq!(out.body["messages"][2]["role"], "assistant");
    }

    /// A client that explicitly sets `stream_options` (even to opt out of
    /// usage) owns that decision — the proxy must not override it.
    #[tokio::test]
    async fn passthrough_preserves_explicit_client_stream_options() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "messages": [{"role":"user","content":"ping"}],
                "stream": true,
                "stream_options": {"include_usage": false}
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["stream_options"]["include_usage"], false,
            "explicit client stream_options must be preserved verbatim",
        );
    }

    /// Non-streaming requests carry `usage` in the regular response body, so
    /// there is nothing to inject — and we must not pollute the body.
    fn provider_with_channel(api_key: &str, channel: Option<&str>, fast_mode: bool) -> Provider {
        let mut provider = provider_with_api_key(api_key);
        provider.channel = channel.map(str::to_string);
        provider.fast_mode = fast_mode;
        provider
    }

    fn responses_ctx<'a>(provider: &'a Provider, gw: &'a Gateway) -> ProviderCtx<'a> {
        responses_ctx_model(provider, gw, "gpt-test")
    }

    /// Responses 直通上下文（actual_model 可指定）：passthrough_run 会用
    /// 它覆盖请求体 model 字段，模型级策略测试需要真实模型名。
    fn responses_ctx_model<'a>(
        provider: &'a Provider,
        gw: &'a Gateway,
        actual_model: &'a str,
    ) -> ProviderCtx<'a> {
        ProviderCtx {
            provider,
            protocol: OPENAI_RESPONSES_V1,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model,
            force_max_reasoning: false,
            credential: None,
            gw,
            disable_default_auth: false,
        }
    }

    /// sub2api Fast 模式的核心行为：Responses 直通请求缺 service_tier 时
    /// 注入 "priority"（OpenAI 官方 Fast mode 语义）。
    #[tokio::test]
    async fn passthrough_injects_service_tier_priority_for_sub2api_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping", "stream": true }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "sub2api fast mode must inject service_tier=priority",
        );
    }

    /// Codex OAuth 渠道复用相同 Fast 模式语义。
    #[tokio::test]
    async fn passthrough_injects_service_tier_priority_for_codex_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("", Some("codex"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "gpt-5-codex", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "codex fast mode must inject service_tier=priority",
        );
    }

    /// Codex 消费级上游（codex 直连 / sub2api 中转）：直通路径剥离其拒绝的
    /// max_output_tokens/temperature/top_p。
    #[tokio::test]
    async fn passthrough_strips_rejected_params_for_codex_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);

            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "gpt-5-codex",
                    "input": "ping",
                    "max_output_tokens": 4096,
                    "temperature": 0.7,
                    "top_p": 0.9,
                    "stream": true,
                }),
                &ctx,
                true,
            )
            .await
            .expect("passthrough succeeds");

            assert!(
                out.body.get("max_output_tokens").is_none(),
                "channel={channel}: consumer backend must not receive max_output_tokens",
            );
            assert!(
                out.body.get("temperature").is_none(),
                "channel={channel}: consumer backend must not receive temperature",
            );
            assert!(
                out.body.get("top_p").is_none(),
                "channel={channel}: consumer backend must not receive top_p",
            );
            assert_eq!(
                out.body["stream"], true,
                "channel={channel}: stream must be preserved",
            );
        }
    }

    #[test]
    fn sanitize_tool_call_ids_unit_cases() {
        let provider = provider_with_channel("", Some("codex"), false);

        // 外来 custom_tool_call（fc_call_ 前缀）被改写为 ctc_ 前缀；
        // call_id 与 output 项均不受影响。
        let mut body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "id": "fc_call_71a12a780e464146864664b5",
                 "name": "exec", "call_id": "call_71a12a780e464146864664b5"},
                {"type": "custom_tool_call_output", "id": "ctco_01a038f6-400b",
                 "call_id": "call_71a12a780e464146864664b5"},
                {"type": "custom_tool_call", "id": "ctc_0031496ce5c2", "name": "exec",
                 "call_id": "call_Wrnr"}
            ]
        });
        sanitize_codex_tool_call_ids(&mut body, &provider);
        let items = body["input"].as_array().unwrap();
        assert_eq!(
            items[0]["id"], "ctc_fc_call_71a12a780e464146864664b5",
            "foreign custom_tool_call id gains the ctc prefix",
        );
        assert_eq!(
            items[0]["call_id"], "call_71a12a780e464146864664b5",
            "call_id is the pairing key and must stay untouched",
        );
        assert_eq!(
            items[1]["id"], "ctco_01a038f6-400b",
            "output item ids are already legal and untouched",
        );
        assert_eq!(
            items[2]["id"], "ctc_0031496ce5c2",
            "native ctc_ ids are left alone",
        );

        // 外来 function_call（非 fc 前缀）同理规范化。
        let mut body = serde_json::json!({
            "input": [
                {"type": "function_call", "id": "call_9f8e7d", "name": "wait", "call_id": "call_9f8e7d"}
            ]
        });
        sanitize_codex_tool_call_ids(&mut body, &provider);
        assert_eq!(
            body["input"][0]["id"], "fc_call_9f8e7d",
            "foreign function_call id gains the fc prefix",
        );

        // 非消费级渠道：不改写。
        let mut body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "id": "fc_call_x", "name": "exec", "call_id": "call_x"}
            ]
        });
        sanitize_codex_tool_call_ids(
            &mut body,
            &provider_with_channel("", Some("default"), false),
        );
        assert_eq!(
            body["input"][0]["id"], "fc_call_x",
            "non-consumer channels are untouched",
        );

        // sub2api 中转与 codex 同契约：外来 ID 同样规范化。
        let mut body = serde_json::json!({
            "input": [
                {"type": "function_call", "id": "call_9f8e7d", "name": "wait", "call_id": "call_9f8e7d"}
            ]
        });
        sanitize_codex_tool_call_ids(
            &mut body,
            &provider_with_channel("sk-sub2api", Some("sub2api"), false),
        );
        assert_eq!(
            body["input"][0]["id"], "fc_call_9f8e7d",
            "sub2api relay shares the codex consumer contract",
        );

        // 缺 id 的调用项：跳过（无 id 无从校验前缀）。
        let mut body = serde_json::json!({
            "input": [{"type": "custom_tool_call", "name": "exec", "call_id": "call_y"}]
        });
        sanitize_codex_tool_call_ids(&mut body, &provider);
        assert!(
            body["input"][0].get("id").is_none(),
            "missing id stays missing",
        );
    }

    #[test]
    fn sanitize_reasoning_content_unit_cases() {
        // 非消费级渠道：不做任何改动。
        let mut body = serde_json::json!({
            "input": [{"type": "reasoning", "content": [{"text": "x"}]}]
        });
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("default"), false),
        );
        assert_eq!(
            body["input"][0]["content"].as_array().map(Vec::len),
            Some(1),
            "non-consumer channels must keep reasoning content untouched",
        );

        // sub2api 中转与 codex 同契约：非空 content 同样剥掉。
        let mut body = serde_json::json!({
            "input": [{"type": "reasoning", "content": [{"text": "x"}]}]
        });
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("sk-sub2api", Some("sub2api"), false),
        );
        assert!(
            body["input"][0].get("content").is_none(),
            "sub2api relay shares the codex consumer contract",
        );

        // codex 渠道：空数组 content 保留（上游允许长度 0），非空剥掉。
        let mut body = serde_json::json!({
            "input": [
                {"type": "reasoning", "content": []},
                {"type": "reasoning", "content": [{"text": "x"}]},
                {"type": "reasoning", "content": "plain-text"},
                {"type": "reasoning"},
                {"type": "reasoning", "content": null}
            ]
        });
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("codex"), false),
        );
        assert!(
            body["input"][0].get("content").is_some(),
            "empty array is legal (max length 0)"
        );
        assert!(
            body["input"][1].get("content").is_none(),
            "non-empty array must be stripped"
        );
        assert!(
            body["input"][2].get("content").is_none(),
            "non-array content must be stripped"
        );
        assert!(
            body["input"][3].get("content").is_none(),
            "absent stays absent"
        );
        assert!(
            body["input"][4].get("content").is_some(),
            "null content stays"
        );

        // input 为字符串简写 / 缺失：不 panic、不改写。
        let mut body = serde_json::json!({"input": "hi"});
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("codex"), false),
        );
        assert_eq!(body["input"], "hi");
        let mut body = serde_json::json!({"model": "m"});
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("codex"), false),
        );
        assert_eq!(body["model"], "m");
    }

    #[test]
    fn sanitize_drops_unresolvable_reasoning_items_when_stateless() {
        let provider = provider_with_channel("", Some("codex"), false);

        // store=false：无 usable encrypted_content 的 reasoning 项整项剔除，
        // 原生项（带 encrypted_content）与非 reasoning 项保留。
        let mut body = serde_json::json!({
            "store": false,
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hi"}
                ]},
                {"type": "reasoning", "id": "rs_resp_20260825194117052ba4e951a74c39",
                 "summary": [], "encrypted_content": null,
                 "content": [{"type": "reasoning_text", "text": "foreign"}]},
                {"type": "reasoning", "id": "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55",
                 "summary": [], "encrypted_content": "enc-keep"},
                {"type": "function_call", "id": "fc_call_x", "name": "f", "arguments": "{}"}
            ]
        });
        sanitize_codex_reasoning_content(&mut body, &provider);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 3, "foreign reasoning item must be dropped");
        assert_eq!(
            items[1]["id"],
            "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55"
        );
        assert_eq!(items[1]["encrypted_content"], "enc-keep");
        assert_eq!(
            items[2]["type"], "function_call",
            "non-reasoning items survive"
        );

        // store=true：客户端显式依赖服务端状态，不剔除，仅剥 content。
        let mut body = serde_json::json!({
            "store": true,
            "input": [{"type": "reasoning", "id": "rs_x", "content": [{"text": "t"}]}]
        });
        sanitize_codex_reasoning_content(&mut body, &provider);
        assert_eq!(
            body["input"].as_array().unwrap().len(),
            1,
            "store=true keeps the item",
        );
        assert!(
            body["input"][0].get("content").is_none(),
            "content strip still applies under store=true",
        );

        // 空白 encrypted_content 字符串同样视为不可重建。
        let mut body = serde_json::json!({
            "store": false,
            "input": [{"type": "reasoning", "encrypted_content": "  "}]
        });
        sanitize_codex_reasoning_content(&mut body, &provider);
        assert!(
            body["input"].as_array().unwrap().is_empty(),
            "blank encrypted_content is not usable",
        );

        // 非消费级渠道：store=false 也不剔除。
        let mut body = serde_json::json!({
            "store": false,
            "input": [{"type": "reasoning", "content": [{"text": "x"}]}]
        });
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("default"), false),
        );
        assert_eq!(
            body["input"].as_array().unwrap().len(),
            1,
            "non-consumer channels are untouched",
        );
    }

    /// 密文存在但 ID 非原生形态（中转铸造，UUID 方言 `rs_<uuid>`）：
    /// codex 消费级后端无法解密，回放即 400 invalid_encrypted_content
    /// （线上请求 06fbe5e2，2026-08-27）。无状态下同样整项剔除。
    #[test]
    fn sanitize_drops_foreign_minted_reasoning_even_with_encrypted_content() {
        let provider = provider_with_channel("", Some("codex"), false);

        // 线上事故原样数据：UUID 方言 + 带密文 -> 剔除；时间戳方言同理；
        // 原生 64 位十六进制 ID 与无 ID 项保留；非 reasoning 项不动。
        let mut body = serde_json::json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                 "summary": [{"type": "summary_text", "text": "relay thought"}],
                 "encrypted_content": "TrvvClVDcSLW/nEEAMKLKvGP"},
                {"type": "reasoning", "id": "rs_resp_202608272326364fc74a543d994008",
                 "summary": [], "encrypted_content": "relay-enc"},
                {"type": "reasoning", "id": "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55",
                 "summary": [], "encrypted_content": "enc-native"},
                {"type": "reasoning", "summary": [], "encrypted_content": "enc-idless"},
                {"type": "message", "role": "assistant",
                 "id": "msg_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                 "content": [{"type": "output_text", "text": "ok"}]}
            ]
        });
        sanitize_codex_reasoning_content(&mut body, &provider);
        let items = body["input"].as_array().unwrap();
        assert_eq!(
            items.len(),
            3,
            "foreign-minted reasoning items must be dropped even with encrypted_content",
        );
        assert_eq!(items[0]["encrypted_content"], "enc-native");
        assert_eq!(items[1]["encrypted_content"], "enc-idless");
        assert_eq!(items[2]["type"], "message", "non-reasoning items survive");

        // store=true：客户端显式依赖服务端状态，ID 形态不剔除。
        let mut body = serde_json::json!({
            "store": true,
            "input": [{"type": "reasoning", "id": "rs_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                       "encrypted_content": "relay-enc"}]
        });
        sanitize_codex_reasoning_content(&mut body, &provider);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);

        // 非消费级渠道：UUID 方言原样透传（普通中转上游自会解自己的密文）。
        let mut body = serde_json::json!({
            "store": false,
            "input": [{"type": "reasoning", "id": "rs_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                       "encrypted_content": "relay-enc"}]
        });
        sanitize_codex_reasoning_content(
            &mut body,
            &provider_with_channel("", Some("default"), false),
        );
        assert_eq!(body["input"].as_array().unwrap().len(), 1);

        // 原生形态判据：64 位十六进制；短长度 / 时间戳 / 非 rs_ 前缀皆否。
        assert!(super::is_native_codex_reasoning_id(
            "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55"
        ));
        assert!(!super::is_native_codex_reasoning_id(
            "rs_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55"
        ));
        assert!(!super::is_native_codex_reasoning_id("rs_033bf81503e4a9e"));
        assert!(!super::is_native_codex_reasoning_id(
            "rs_resp_202608272326364fc74a543d994008"
        ));
        assert!(!super::is_native_codex_reasoning_id("resp_033b"));
    }

    /// Codex 消费级上游（codex 直连 / sub2api 中转）：直通路径剥离 reasoning
    /// 项的非空 content。完整复现线上 400 现场：回放历史携带
    /// `reasoning.content` 文本数组。
    #[tokio::test]
    async fn passthrough_strips_reasoning_content_for_codex_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);

            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "gpt-5.6-sol",
                    "stream": true,
                    "include": ["reasoning.encrypted_content"],
                    "input": [
                        {"type": "message", "role": "user", "content": [
                            {"type": "input_text", "text": "hi"}
                        ]},
                        {"type": "reasoning", "summary": [], "encrypted_content": "enc-1", "content": [
                            {"text": "We need continue task. Need inspect files."}
                        ]},
                        {"type": "agent_message", "content": [
                            {"type": "output_text", "text": "ok"}
                        ]}
                    ]
                }),
                &ctx,
                true,
            )
            .await
            .expect("passthrough succeeds");

            let reasoning = &out.body["input"][1];
            assert!(
                reasoning.get("content").is_none(),
                "channel={channel}: consumer backend must not receive non-empty reasoning.content",
            );
            assert_eq!(
                reasoning["encrypted_content"], "enc-1",
                "channel={channel}: encrypted_content must survive the strip",
            );
            assert_eq!(
                out.body["input"][0]["content"].as_array().map(Vec::len),
                Some(1),
                "channel={channel}: message item content must be untouched",
            );
            assert_eq!(
                out.body["input"][2]["content"].as_array().map(Vec::len),
                Some(1),
                "channel={channel}: agent_message item content must be untouched",
            );
        }
    }

    /// Codex 消费级上游（codex 直连 / sub2api 中转）：直通路径规范化外来
    /// 工具调用 ID 前缀。完整复现线上 400 现场：多后端路由切到中转再切回，
    /// 中转轮的 custom_tool_call 带 `fc_call_` ID，回放给 codex 触发
    /// `Expected an ID that begins with 'ctc'`。
    #[tokio::test]
    async fn passthrough_normalizes_foreign_tool_call_ids_for_codex_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);

            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "gpt-5.6-sol",
                    "stream": true,
                    "store": false,
                    "input": [
                        {"type": "reasoning", "id": "rs_resp_202608252046531fe8808638bd475f",
                         "summary": [], "encrypted_content": null,
                         "content": [{"type": "reasoning_text", "text": "foreign"}]},
                        {"type": "custom_tool_call", "id": "fc_call_71a12a780e464146864664b5",
                         "name": "exec", "call_id": "call_71a12a780e464146864664b5",
                         "input": "ls"},
                        {"type": "custom_tool_call_output",
                         "id": "ctco_01a038f6-400b-7f83-b64e-fb3a8b8c7ed5",
                         "call_id": "call_71a12a780e464146864664b5",
                         "output": [{"type": "input_text", "text": "done"}]}
                    ]
                }),
                &ctx,
                true,
            )
            .await
            .expect("passthrough succeeds");

            // Foreign reasoning dropped by the stateless rule; the foreign
            // custom_tool_call survives with a normalized ctc_ id and its
            // call_id pairing intact.
            let items = out.body["input"].as_array().unwrap();
            assert_eq!(
                items.len(),
                2,
                "channel={channel}: foreign reasoning is dropped"
            );
            assert_eq!(
                items[0]["id"], "ctc_fc_call_71a12a780e464146864664b5",
                "channel={channel}: foreign tool call id is normalized to the ctc prefix",
            );
            assert_eq!(
                items[0]["call_id"], "call_71a12a780e464146864664b5",
                "channel={channel}: pairing call_id stays untouched",
            );
            assert_eq!(
                items[1]["type"], "custom_tool_call_output",
                "channel={channel}: the paired output survives next to its (renamed) call",
            );
        }
    }

    /// Codex 消费级上游（codex 直连 / sub2api 中转）：store=false 时整项剔除
    /// 无法无状态重建的外来 reasoning 项。完整复现线上 404 现场：多后端路由
    /// 中途切换 provider，中转铸造的 reasoning 项（未知 ID + null
    /// encrypted_content）回放到 codex 直连后端触发 `Item with id ... not found`。
    #[tokio::test]
    async fn passthrough_drops_foreign_reasoning_items_for_stateless_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);

            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "gpt-5.6-sol",
                    "stream": true,
                    "store": false,
                    "include": ["reasoning.encrypted_content"],
                    "input": [
                        {"type": "message", "role": "user", "content": [
                            {"type": "input_text", "text": "hi"}
                        ]},
                        {"type": "reasoning", "id": "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55",
                         "summary": [], "encrypted_content": "enc-native"},
                        {"type": "reasoning", "id": "rs_resp_20260825194117052ba4e951a74c39",
                         "summary": [], "encrypted_content": null,
                         "content": [{"type": "reasoning_text", "text": "foreign relay thought"}]},
                        {"type": "function_call", "id": "fc_call_65b6bc94615d4438a72cf450",
                         "name": "f", "arguments": "{}"}
                    ]
                }),
                &ctx,
                true,
            )
            .await
            .expect("passthrough succeeds");

            let items = out.body["input"].as_array().unwrap();
            assert_eq!(
                items.len(),
                3,
                "channel={channel}: foreign reasoning item (unknown id, null encrypted_content) must be dropped",
            );
            assert_eq!(
                items[1]["encrypted_content"], "enc-native",
                "channel={channel}: native reasoning replay survives",
            );
            assert_eq!(
                items[2]["type"], "function_call",
                "channel={channel}: non-reasoning items survive",
            );
            assert_eq!(
                out.body["store"], false,
                "channel={channel}: store flag is untouched",
            );
        }
    }

    /// 完整复现线上 400 invalid_encrypted_content 现场（请求 06fbe5e2，
    /// 2026-08-27）：会话中途从中转切到 codex 直连，回放历史携带 13 项
    /// 中转铸造的 UUID 方言 reasoning 项（密文存在但本后端不可解密），
    /// 首个即被上游拒收。防御后：外来密文项整项剔除，请求可通过。
    /// sub2api 中转与 codex 直连同后端契约，同样生效。
    #[tokio::test]
    async fn passthrough_drops_foreign_minted_reasoning_for_stateless_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);

            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "gpt-5.6-sol",
                    "stream": true,
                    "store": false,
                    "include": ["reasoning.encrypted_content"],
                    "input": [
                        {"type": "message", "role": "user", "content": [
                            {"type": "input_text", "text": "hi"}
                        ]},
                        {"type": "reasoning", "id": "rs_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                         "summary": [{"type": "summary_text", "text":
                           "The user is asking what model I am."}],
                         "encrypted_content": "TrvvClVDcSLW/nEEAMKLKvGP"},
                        {"type": "message", "role": "assistant",
                         "id": "msg_a44b4856-06b6-9e52-86c8-9d9d0c1a5d55",
                         "content": [{"type": "output_text", "text": "ok"}]},
                        {"type": "reasoning", "id": "rs_a44b485606b69e5286c89d9d0c1a5d55a44b485606b69e5286c89d9d0c1a5d55",
                         "summary": [], "encrypted_content": "enc-native"}
                    ]
                }),
                &ctx,
                true,
            )
            .await
            .expect("passthrough succeeds");

            let items = out.body["input"].as_array().unwrap();
            assert_eq!(
                items.len(),
                3,
                "channel={channel}: foreign-minted reasoning (UUID dialect) must be dropped",
            );
            assert_eq!(
                items[0]["type"], "message",
                "channel={channel}: the relay-minted reasoning item ahead of the message is gone",
            );
            assert_eq!(
                items[2]["encrypted_content"], "enc-native",
                "channel={channel}: native reasoning replay survives",
            );
        }
    }

    /// 非消费级渠道必须保留这些参数，直通行为不受影响。
    #[tokio::test]
    async fn passthrough_keeps_rejected_params_for_other_channels() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-other", Some("default"), false);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "o3",
                "input": "ping",
                "max_output_tokens": 2048,
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["max_output_tokens"], 2048,
            "non-consumer channel must keep max_output_tokens",
        );
    }

    /// IR 转码路径同样剥离：消费级渠道（codex 直连 / sub2api 中转）encode
    /// 后清掉被拒绝的参数。
    #[tokio::test]
    async fn build_request_strips_rejected_params_for_codex_consumer_channels() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api"] {
            let provider = provider_with_channel("", Some(channel), false);
            let ctx = responses_ctx(&provider, &gw);
            let mut req = minimal_chat_request();

            let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
                .await
                .expect("build_request succeeds");

            assert!(
                out.body.get("max_output_tokens").is_none(),
                "channel={channel}: IR transcode path must strip max_output_tokens",
            );
            assert!(
                out.body.get("temperature").is_none(),
                "channel={channel}: IR transcode path must strip temperature",
            );
            assert!(
                out.body.get("top_p").is_none(),
                "channel={channel}: IR transcode path must strip top_p",
            );
        }
    }

    /// 客户端显式携带的 service_tier 拥有最终决定权，Fast 模式不得覆盖。
    #[tokio::test]
    async fn passthrough_preserves_explicit_client_service_tier() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "o3",
                "input": "ping",
                "service_tier": "auto",
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["service_tier"], "auto",
            "explicit client service_tier must be preserved verbatim",
        );
    }

    /// Fast 模式开关关闭时不得注入任何字段。
    #[tokio::test]
    async fn passthrough_skips_service_tier_when_fast_mode_disabled() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), false);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("service_tier").is_none(),
            "fast mode off must not inject service_tier",
        );
    }

    /// Fast 模式只属于 codex/sub2api 消费级渠道：其他渠道开启该标志也不注入。
    #[tokio::test]
    async fn passthrough_skips_service_tier_for_other_channels() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-other", Some("default"), true);
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "model": "o3", "input": "ping" }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("service_tier").is_none(),
            "non-consumer channel must not receive service_tier injection",
        );
    }

    /// IR 转码路径（build_request）同样注入：post_encode 之后补 priority。
    #[tokio::test]
    async fn build_request_injects_service_tier_for_sub2api_fast_mode() {
        let gw = build_test_gateway().await;
        let provider = provider_with_channel("sk-sub2api", Some("sub2api"), true);
        let ctx = responses_ctx(&provider, &gw);
        let mut req = minimal_chat_request();

        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        assert_eq!(
            out.body["service_tier"], "priority",
            "IR transcode path must inject service_tier=priority for sub2api fast mode",
        );
    }

    fn provider_with_vendor(api_key: &str, vendor: Option<&str>) -> Provider {
        let mut provider = provider_with_api_key(api_key);
        provider.vendor = vendor.map(str::to_string);
        provider
    }

    fn ark_provider(api_key: &str, vendor: Option<&str>) -> Provider {
        let mut provider = provider_with_vendor(api_key, vendor);
        provider.base_url = "https://ark.cn-beijing.volces.com/api/coding/v3".into();
        provider
    }

    /// grok 判定优先看请求体模型名：vendor=custom 的中继 provider 转发
    /// grok-* 模型时也必须删字段（grok 拒收 none）。
    #[tokio::test]
    async fn passthrough_drops_effort_for_grok_model_via_relay_with_custom_vendor() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-relay", Some("custom"));
        let ctx = openai_chat_ctx(&provider, &gw, "grok-4.6-build");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "grok-4.6-build",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "none"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("reasoning_effort").is_none(),
            "grok-* model via custom-vendor relay must have reasoning_effort dropped",
        );
    }

    /// 线上事故复现（请求 65fffc9a，2026-08-26）：grok-4.6 经
    /// cli-chat-proxy /v1/responses 对 reasoning.effort="max" 返回 400
    /// {"code":"invalid-argument","error":"Invalid reasoning effort."}
    /// （4/4 全复现）。max 必须降级为 grok 顶格档 xhigh。
    #[tokio::test]
    async fn passthrough_downgrades_max_effort_to_xhigh_for_grok() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-grok", Some("xai"));
        let ctx = responses_ctx(&provider, &gw);

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "grok-4.6",
                "input": "ping",
                "reasoning": {"effort": "max"}
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["reasoning"]["effort"], "xhigh",
            "grok rejects max (400 Invalid reasoning effort); must downgrade to xhigh",
        );
    }

    /// Chat wire shape 的 max 降级：top-level reasoning_effort。
    #[tokio::test]
    async fn passthrough_downgrades_max_effort_to_xhigh_for_grok_chat_shape() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-relay", Some("custom"));
        let ctx = openai_chat_ctx(&provider, &gw, "grok-4.6");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "grok-4.6",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "max"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["reasoning_effort"], "xhigh",
            "chat-shape max must also downgrade to xhigh for grok-* models",
        );
    }

    /// vendor=xai 的直连 provider 同样删字段（原有行为不回归）。
    #[tokio::test]
    async fn passthrough_drops_effort_for_xai_vendor_direct() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-xai", Some("xai"));
        let ctx = openai_chat_ctx(&provider, &gw, "grok-4.5");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "grok-4.5",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "disable"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("reasoning_effort").is_none(),
            "xai vendor direct must drop every off spelling",
        );
    }

    /// 非 grok 模型经 custom vendor 走归一路径：disable → none。
    /// （glm-4.6：未登记 THINKING_MANDATORY_MODELS，历史上接受 none。）
    #[tokio::test]
    async fn passthrough_normalizes_off_spellings_for_non_grok_models() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-glm", Some("zhipuai"));
        let ctx = openai_chat_ctx(&provider, &gw, "glm-4.6");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "glm-4.6",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "disabled"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["reasoning_effort"], "none",
            "non-grok model must normalize off spellings to none",
        );
    }

    /// 模型映射级「max推理」（models.force_max_reasoning）单测：IR 覆盖
    /// ①显式档位 ②未携带 ③关闭/预算制三种形态，display 偏好保留。
    #[test]
    fn force_max_reasoning_ir_overrides_every_client_directive() {
        let mut req = crate::protocol::ir::AiRequest::new("m", Vec::new());
        // ① 显式档位
        req.reasoning.effort = Some(crate::protocol::ir::ReasoningEffort::Low);
        force_max_reasoning_ir(&mut req);
        assert_eq!(
            req.reasoning.effort,
            Some(crate::protocol::ir::ReasoningEffort::Max)
        );
        // ② 未携带
        let mut req = crate::protocol::ir::AiRequest::new("m", Vec::new());
        force_max_reasoning_ir(&mut req);
        assert!(req.reasoning.enabled);
        assert_eq!(
            req.reasoning.effort,
            Some(crate::protocol::ir::ReasoningEffort::Max)
        );
        // ③ 预算制被清除（budget 表达不了 max）
        let mut req = crate::protocol::ir::AiRequest::new("m", Vec::new());
        req.reasoning.budget_tokens = Some(8192);
        req.reasoning.display = Some("detailed".to_string());
        force_max_reasoning_ir(&mut req);
        assert!(req.reasoning.budget_tokens.is_none());
        assert_eq!(req.reasoning.display.as_deref(), Some("detailed"));
    }

    /// 直通 wire 级覆盖的四种协议形态（与各 IR 编码器的 max 表达一致）。
    #[test]
    fn apply_force_max_reasoning_body_covers_all_protocols() {
        use crate::protocol::ids::Protocol;
        // chat：顶层 reasoning_effort
        let mut body = serde_json::json!({"model": "m"});
        apply_force_max_reasoning_body(&mut body, Protocol::OpenAICompatible);
        assert_eq!(body["reasoning_effort"], "max");

        // Responses：嵌套 effort，兄弟键保留
        let mut body = serde_json::json!({"reasoning": {"summary": "auto"}});
        apply_force_max_reasoning_body(&mut body, Protocol::OpenAIResponses);
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["reasoning"]["summary"], "auto");
        let mut body = serde_json::json!({"model": "m"});
        apply_force_max_reasoning_body(&mut body, Protocol::OpenAIResponses);
        assert_eq!(body["reasoning"]["effort"], "max");

        // Anthropic：thinking adaptive + output_config.effort（客户端显式
        // 关闭形态被覆盖——会话决策 ③）
        let mut body = serde_json::json!({"thinking": {"type": "disabled"}});
        apply_force_max_reasoning_body(&mut body, Protocol::AnthropicMessages);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");

        // Gemini：顶格档是 high（google_thinking_level(Max)）
        let mut body = serde_json::json!({"model": "m"});
        apply_force_max_reasoning_body(&mut body, Protocol::GoogleGemini);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    /// 直通集成：force_max_reasoning 覆盖客户端显式 low 档为 max；且
    /// vendor 安全网随后照常裁决——grok 后端 max 降级 xhigh（线上事故
    /// 65fffc9a 的兼容性不回归）。
    #[tokio::test]
    async fn passthrough_forces_max_reasoning_for_route_override() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");

        // 显式 low → 强制 max
        let ctx = openai_chat_ctx_forced(&provider, &gw, "glm-4.6");
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "glm-4.6",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "low"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(
            out.body["reasoning_effort"], "max",
            "route-level force_max_reasoning must override the client tier",
        );

        // 未携带推理指令 → 注入 max
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "glm-4.6",
                "messages": [{"role":"user","content":"ping"}]
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(out.body["reasoning_effort"], "max");

        // vendor 安全网：grok 模型强制 max 后按方言降级 xhigh
        let grok_ctx = openai_chat_ctx_forced(&provider, &gw, "grok-4.6");
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "grok-4.6",
                "messages": [{"role":"user","content":"ping"}]
            }),
            &grok_ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(
            out.body["reasoning_effort"], "xhigh",
            "vendor effort policy must still adjudicate after the override",
        );
    }

    /// 转码路径全协议矩阵：dispatcher 在 build_request 之前注入 IR（见
    /// force_max_reasoning_ir），四种 egress 协议的编码器必须各自原生表达
    /// max 档——chat 顶层 reasoning_effort、Responses 嵌套 reasoning.effort、
    /// Anthropic thinking.adaptive + output_config.effort、Gemini 顶格
    /// thinkingLevel=high（google_thinking_level(Max)）。
    #[tokio::test]
    async fn force_max_reasoning_reaches_every_egress_protocol_on_transcode() {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        };
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");

        let cases = [
            (OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, "chat"),
            (OPENAI_RESPONSES_V1, "responses"),
            (ANTHROPIC_MESSAGES_2023_06_01, "anthropic"),
            (GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA, "gemini"),
        ];

        for (protocol, tag) in cases {
            let mut req = minimal_chat_request();
            // 模拟 dispatcher 顺序：解码 → force-max 注入 → 编码。
            force_max_reasoning_ir(&mut req);
            let ctx = ProviderCtx {
                provider: &provider,
                protocol,
                egress_base_url: "https://upstream.local",
                api_key: &provider.api_key,
                auth_scheme: "auto",
                actual_model: "m-test",
                force_max_reasoning: false,
                credential: None,
                gw: &gw,
                disable_default_auth: false,
            };
            let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
                .await
                .expect("build_request succeeds");
            match tag {
                "chat" => assert_eq!(
                    out.body["reasoning_effort"], "max",
                    "chat egress must carry top-level max effort",
                ),
                "responses" => assert_eq!(
                    out.body["reasoning"]["effort"], "max",
                    "responses egress must carry nested max effort",
                ),
                "anthropic" => {
                    assert_eq!(
                        out.body["thinking"]["type"], "adaptive",
                        "anthropic egress must enable adaptive thinking",
                    );
                    assert_eq!(
                        out.body["output_config"]["effort"], "max",
                        "anthropic egress must carry max output_config effort",
                    );
                }
                _ => assert_eq!(
                    out.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high",
                    "gemini egress must carry the top thinking level",
                ),
            }
        }
    }

    /// 直通路径全协议矩阵（chat 形态已由上一测试覆盖）：Responses 嵌套档位、
    /// Anthropic 关闭形态被覆盖、Gemini 顶格档，均须在保持逐字直通的前提
    /// 下于 wire 级改写成功，兄弟键保留。
    #[tokio::test]
    async fn passthrough_forces_max_reasoning_for_every_native_protocol() {
        use crate::protocol::ids::{
            ANTHROPIC_MESSAGES_2023_06_01, GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
        };
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");

        // Responses：嵌套 effort 改写，summary 兄弟键保留。
        let ctx = ProviderCtx {
            force_max_reasoning: true,
            ..responses_ctx(&provider, &gw)
        };
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "gpt-test",
                "input": "ping",
                "reasoning": {"effort": "low", "summary": "auto"}
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(out.body["reasoning"]["effort"], "max");
        assert_eq!(out.body["reasoning"]["summary"], "auto");

        // Anthropic：显式 disabled 被覆盖为 adaptive + max（会话决策 ③）。
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: ANTHROPIC_MESSAGES_2023_06_01,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "claude-test",
            force_max_reasoning: true,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "claude-test",
                "max_tokens": 128,
                "thinking": {"type": "disabled"},
                "messages": [{"role": "user", "content": "ping"}]
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(out.body["thinking"]["type"], "adaptive");
        assert_eq!(out.body["output_config"]["effort"], "max");

        // Gemini：thinkingConfig 顶格 high（Max 在 Gemini 无对应档）。
        let ctx = ProviderCtx {
            provider: &provider,
            protocol: GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            egress_base_url: "https://upstream.local",
            api_key: &provider.api_key,
            auth_scheme: "auto",
            actual_model: "gemini-test",
            force_max_reasoning: true,
            credential: None,
            gw: &gw,
            disable_default_auth: false,
        };
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "gemini-test",
                "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
                "generationConfig": {"temperature": 0.7}
            }),
            &ctx,
            true,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(
            out.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high",
        );
        assert_eq!(
            out.body["generationConfig"]["temperature"], 0.7,
            "sibling generationConfig keys must survive",
        );
    }

    /// Sanitized reproduction of bcd2a3ed-af4b-44d7-8b94-3c7c29876396:
    /// Codex rejected gpt-6-astra + reasoning.effort=none with unsupported_value.
    #[tokio::test]
    async fn astra_responses_passthrough_clamps_off_and_preserves_supported_tiers() {
        let gw = build_test_gateway().await;
        for channel in ["codex", "sub2api", "custom"] {
            let provider = provider_with_channel("sk-test", Some(channel), false);
            let ctx = responses_ctx_model(&provider, &gw, "gpt-6-astra");
            for raw in [
                "none", "disable", "disabled", "off", "low", "medium", "high", "xhigh", "max",
            ] {
                let expected = if matches!(raw, "none" | "disable" | "disabled" | "off") {
                    "low"
                } else {
                    raw
                };
                let out = passthrough_run(
                    &FakeApiKeyVendor,
                    serde_json::json!({
                        "model": "gpt-6-astra",
                        "input": "ping",
                        "reasoning": {"effort": raw, "summary": "auto"},
                        "stream": true
                    }),
                    &ctx,
                    true,
                )
                .await
                .expect("passthrough succeeds");
                assert_eq!(
                    out.body["reasoning"]["effort"], expected,
                    "{channel}: {raw}"
                );
                assert_eq!(out.body["reasoning"]["summary"], "auto");
                assert_eq!(out.body["input"], "ping");
            }
        }
    }

    #[tokio::test]
    async fn astra_chat_and_ir_paths_clamp_using_actual_upstream_model() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-test", Some("custom"));
        let chat_ctx = openai_chat_ctx(&provider, &gw, "gpt-6-astra");
        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "client-alias",
                "messages": [{"role": "user", "content": "ping"}],
                "reasoning_effort": "none"
            }),
            &chat_ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");
        assert_eq!(out.body["model"], "gpt-6-astra");
        assert_eq!(out.body["reasoning_effort"], "low");

        for ctx in [chat_ctx, responses_ctx_model(&provider, &gw, "gpt-6-astra")] {
            let mut req = minimal_chat_request();
            req.reasoning.effort = Some(ReasoningEffort::None);
            let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
                .await
                .expect("build_request succeeds");
            let effort = if ctx.protocol == OPENAI_RESPONSES_V1 {
                &out.body["reasoning"]["effort"]
            } else {
                &out.body["reasoning_effort"]
            };
            assert_eq!(effort, "low");
        }
    }

    #[test]
    fn astra_effort_policy_only_matches_verified_model() {
        let provider = provider_with_vendor("sk-test", Some("custom"));
        for (model, expected) in [
            ("gpt-6-astra", "low"),
            (" GPT-6-ASTRA ", "low"),
            ("gpt-6", "none"),
            ("gpt-5", "none"),
            ("gpt-6-astra-other", "none"),
            ("gpt-6-astral", "none"),
        ] {
            let mut body = serde_json::json!({
                "model": model, "reasoning_effort": "none", "reasoning": {"effort": "none"}
            });
            super::apply_vendor_effort_policy(&mut body, &provider);
            assert_eq!(body["reasoning_effort"], expected, "{model}");
            assert_eq!(body["reasoning"]["effort"], expected, "{model}");
        }
    }

    /// 线上事故复现（请求 58e799fa，2026-08-26）：DSH 发 misspelling
    /// "disable"，默认 normalize 会归一成 "none"，被 ark glm-5.3 以
    /// 400 InvalidParameter 拒收。正确拼写的 "none" 同样拒收。
    /// 两个形态都必须在 wire 边界钳制为 low。
    #[tokio::test]
    async fn passthrough_clamps_off_effort_to_low_for_ark_glm() {
        let gw = build_test_gateway().await;
        let provider = ark_provider("sk-ark", Some("ark-coding"));
        let ctx = openai_chat_ctx(&provider, &gw, "glm-5.3");

        for raw in ["disable", "none", "off"] {
            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "glm-5.3",
                    "messages": [{"role":"user","content":"ping"}],
                    "reasoning_effort": raw
                }),
                &ctx,
                false,
            )
            .await
            .expect("passthrough succeeds");

            assert_eq!(
                out.body["reasoning_effort"], "low",
                "raw={raw} must clamp to low for ark glm-5.3 (none is rejected upstream)",
            );
        }
    }

    /// vendor=custom 的手配 provider 靠 base_url（volces.com）识别为 ark。
    #[tokio::test]
    async fn passthrough_clamps_off_effort_for_custom_vendor_ark_base_url() {
        let gw = build_test_gateway().await;
        let provider = ark_provider("sk-ark", Some("custom"));
        let ctx = openai_chat_ctx(&provider, &gw, "GLM-5.3");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "GLM-5.3",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "disable"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["reasoning_effort"], "low",
            "custom-vendor provider on volces.com base_url must clamp off to low",
        );
    }

    /// 思考强制开启的 GLM 模型（官方文档：thinking.type 仅支持 enabled、
    /// 不支持关闭思考，off 意图「请求将失败」）：off 全形态（含 misspelling）
    /// 在 zhipuai 直连上钳为 low，其余档位原样透传。
    #[tokio::test]
    async fn passthrough_clamps_off_effort_to_low_for_thinking_mandatory_glm() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-glm", Some("zhipuai"));

        for model in ["glm-5.3", "glm-5.3-flash"] {
            let ctx = openai_chat_ctx(&provider, &gw, model);
            for raw in ["none", "disable", "disabled", "off"] {
                let out = passthrough_run(
                    &FakeApiKeyVendor,
                    serde_json::json!({
                        "model": model,
                        "messages": [{"role":"user","content":"ping"}],
                        "reasoning_effort": raw
                    }),
                    &ctx,
                    false,
                )
                .await
                .expect("passthrough succeeds");

                assert_eq!(
                    out.body["reasoning_effort"], "low",
                    "model={model} raw={raw} must clamp to low (thinking cannot be disabled)",
                );
            }

            for raw in ["low", "high", "max"] {
                let out = passthrough_run(
                    &FakeApiKeyVendor,
                    serde_json::json!({
                        "model": model,
                        "messages": [{"role":"user","content":"ping"}],
                        "reasoning_effort": raw
                    }),
                    &ctx,
                    false,
                )
                .await
                .expect("passthrough succeeds");

                assert_eq!(
                    out.body["reasoning_effort"], raw,
                    "model={model} raw={raw} must pass through",
                );
            }

            // 官方三档之外的已知档位：窄化映射（minimal→low；medium/xhigh→high，
            // 保推理质量取向）。
            for (raw, expect) in [("minimal", "low"), ("medium", "high"), ("xhigh", "high")] {
                let out = passthrough_run(
                    &FakeApiKeyVendor,
                    serde_json::json!({
                        "model": model,
                        "messages": [{"role":"user","content":"ping"}],
                        "reasoning_effort": raw
                    }),
                    &ctx,
                    false,
                )
                .await
                .expect("passthrough succeeds");

                assert_eq!(
                    out.body["reasoning_effort"], expect,
                    "model={model} raw={raw} must narrow down to {expect}",
                );
            }
        }
    }

    /// Responses 直通形态：嵌套 reasoning.effort 的 off 意图同样钳为 low，
    /// 且保留 summary 等兄弟键（bigmodel 官方提供 OpenAI Responses 端点）。
    #[tokio::test]
    async fn responses_passthrough_clamps_nested_reasoning_effort_for_glm() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-glm", Some("zhipuai"));

        for model in ["glm-5.3", "glm-5.3-flash"] {
            let ctx = responses_ctx_model(&provider, &gw, model);
            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": model,
                    "input": "ping",
                    "reasoning": {
                        "effort": "disable",
                        "summary": "auto"
                    }
                }),
                &ctx,
                false,
            )
            .await
            .expect("passthrough succeeds");

            assert_eq!(
                out.body["reasoning"]["effort"], "low",
                "nested reasoning.effort off intent must clamp to low for {model}",
            );
            assert_eq!(
                out.body["reasoning"]["summary"], "auto",
                "sibling keys in the reasoning object must be preserved",
            );
        }
    }

    /// 模型名匹配规则：大小写不敏感；带日期等短横线后缀的变体一并覆盖；
    /// 同系已登记模型共享钳制语义，未登记的旧模型不受影响。
    #[tokio::test]
    async fn thinking_mandatory_matching_covers_variants_and_skips_unrelated() {
        let gw = build_test_gateway().await;
        let provider = provider_with_vendor("sk-glm", Some("zhipuai"));

        for model in ["GLM-5.3-Flash", "glm-5.3-flash-0901"] {
            let ctx = openai_chat_ctx(&provider, &gw, model);
            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": model,
                    "messages": [{"role":"user","content":"ping"}],
                    "reasoning_effort": "none"
                }),
                &ctx,
                false,
            )
            .await
            .expect("passthrough succeeds");

            assert_eq!(
                out.body["reasoning_effort"], "low",
                "model={model} is a thinking-mandatory variant and must clamp",
            );
        }

        // 登记表成员与未登记旧模型的边界：glm-5.3 / flash 已登记，glm-4.6 不在。
        assert!(super::is_thinking_mandatory_model("glm-5.3"));
        assert!(super::is_thinking_mandatory_model("GLM-5.3-FLASH"));
        assert!(!super::is_thinking_mandatory_model("glm-4.6"));
    }

    /// 未登记拒收的 ark 模型维持 normalize 方言（disable → none），
    /// 不误伤接受 none 的模型。
    #[tokio::test]
    async fn passthrough_normalizes_off_spellings_for_ark_unverified_models() {
        let gw = build_test_gateway().await;
        let provider = ark_provider("sk-ark", Some("ark-coding"));
        let ctx = openai_chat_ctx(&provider, &gw, "kimi-k2.7-code");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({
                "model": "kimi-k2.7-code",
                "messages": [{"role":"user","content":"ping"}],
                "reasoning_effort": "disable"
            }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert_eq!(
            out.body["reasoning_effort"], "none",
            "ark models without a live none-rejection finding keep the normalize dialect",
        );
    }

    /// 官方合法三档原样透传，不受钳制影响。glm-5.3 属思考强制开启模型
    /// （THINKING_MANDATORY_MODELS）：三档之外的 medium 在此场景按官方枚举
    /// 收窄为 low，由 thinking-mandatory 矩阵测试覆盖。
    #[tokio::test]
    async fn passthrough_keeps_real_effort_levels_for_ark_glm() {
        let gw = build_test_gateway().await;
        let provider = ark_provider("sk-ark", Some("ark-coding"));
        let ctx = openai_chat_ctx(&provider, &gw, "glm-5.3");

        for raw in ["low", "high", "max"] {
            let out = passthrough_run(
                &FakeApiKeyVendor,
                serde_json::json!({
                    "model": "glm-5.3",
                    "messages": [{"role":"user","content":"ping"}],
                    "reasoning_effort": raw
                }),
                &ctx,
                false,
            )
            .await
            .expect("passthrough succeeds");

            assert_eq!(
                out.body["reasoning_effort"], raw,
                "doc-tier raw={raw} passes through the ark clamp untouched",
            );
        }
    }

    /// IR 转码路径（build_request）同样钳制：compat 直通重建的 native_body
    /// 经 vendor patch 携带 none→low 改写到达上游。
    #[tokio::test]
    async fn build_request_clamps_none_effort_to_low_for_ark_glm() {
        let gw = build_test_gateway().await;
        let provider = ark_provider("sk-ark", Some("ark-coding"));
        let ctx = openai_chat_ctx(&provider, &gw, "glm-5.3");
        let mut req = minimal_chat_request();
        req.reasoning.effort = Some(ReasoningEffort::None);

        let out = build_request(&FakeApiKeyVendor, &mut req, &ctx)
            .await
            .expect("build_request succeeds");

        assert_eq!(
            out.body["reasoning_effort"], "low",
            "IR transcode path must clamp none to low for ark glm-5.3",
        );
    }

    #[tokio::test]
    async fn passthrough_skips_include_usage_when_not_streaming() {
        let gw = build_test_gateway().await;
        let provider = provider_with_api_key("apikey-abc");
        let ctx = openai_chat_ctx(&provider, &gw, "gpt-test");

        let out = passthrough_run(
            &FakeApiKeyVendor,
            serde_json::json!({ "messages": [{"role":"user","content":"ping"}] }),
            &ctx,
            false,
        )
        .await
        .expect("passthrough succeeds");

        assert!(
            out.body.get("stream_options").is_none(),
            "non-streaming passthrough must not inject stream_options",
        );
    }
}
