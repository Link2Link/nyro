//! The vision-shim `PhaseHook` — multimodal facade orchestration.
//!
//! Runs in the `OnAccess` phase (route + auth resolved, before any upstream
//! work). For a model whose route carries a vision-shim configuration, every
//! image in the request is transcribed to text by the configured helper model
//! (via [`crate::vision_shim`]) and the image blocks are replaced before the
//! dispatcher builds the outbound request. Mutations made here participate in
//! the existing baseline-diff machinery, so both the IR-translation path and
//! the raw-wire compat path see the rewritten request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::join_all;
use sha2::{Digest, Sha256};

use crate::Gateway;
use crate::error::GatewayError;
use crate::plugin::phase::{Phase, PhaseCtx, PhaseHook, PhaseHookRegistration, PhaseOutcome};
use crate::protocol::ir::AiRequest;
use crate::proxy::context::Deadline;

use super::cache;
use super::caption;
use super::config::{CAPTION_PROMPT_VERSION, VisionShimFailureMode};
use super::ir_scan;

/// Request-scoped vision-shim accounting, published into the request context
/// extensions bag (and the tracing log) whenever the shim touched a request.
#[derive(Debug, Clone, Default)]
pub struct VisionShimStats {
    /// Helper upstream model name (e.g. `glm-5.3-flash`).
    pub helper_model: String,
    /// Total image occurrences found in the request.
    pub images: usize,
    /// Images transcribed by the helper in this request.
    pub captioned: usize,
    /// Images served from the caption cache.
    pub cache_hits: usize,
    /// Images replaced with placeholders (limits, failures, unsupported).
    pub placeholders: usize,
    /// Tokens consumed by helper calls in this request.
    pub helper_tokens: u64,
    /// Wall-clock time spent captioning, in milliseconds.
    pub caption_latency_ms: u64,
}

/// The registered vision-shim phase hook.
pub struct VisionShimHook;

#[async_trait]
impl PhaseHook for VisionShimHook {
    fn name(&self) -> &'static str {
        "vision-shim"
    }

    fn phase(&self) -> Phase {
        Phase::OnAccess
    }

    async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome {
        let deadline = ctx.req_ctx.deadline.clone();
        match process(ctx.host.gateway, ctx.request, &deadline).await {
            Ok(stats) => {
                if stats.images > 0 {
                    tracing::info!(
                        target: "nyro::vision_shim",
                        model = %ctx.request.model,
                        helper = %stats.helper_model,
                        images = stats.images,
                        captioned = stats.captioned,
                        cache_hits = stats.cache_hits,
                        placeholders = stats.placeholders,
                        helper_tokens = stats.helper_tokens,
                        caption_latency_ms = stats.caption_latency_ms,
                        "vision shim applied"
                    );
                    ctx.req_ctx.extensions.insert(stats);
                    // The IR diverged from the client wire body: disable the
                    // native passthrough so the re-encoded request is sent.
                    ctx.req_ctx
                        .extensions
                        .insert(crate::plugin::phase::RequestMutated);
                }
                PhaseOutcome::Continue
            }
            Err(error) => {
                tracing::warn!(error = %error, "vision shim rejected request");
                PhaseOutcome::Reject(error)
            }
        }
    }
}

inventory::submit! {
    PhaseHookRegistration { make: || Arc::new(VisionShimHook) }
}

/// Public test/tool entry: apply the shim with a never-firing deadline.
pub async fn apply(gw: &Gateway, request: &mut AiRequest) -> Result<VisionShimStats, GatewayError> {
    process(gw, request, &Deadline::never()).await
}

/// One pending helper caption job (cache miss).
struct CaptionJob {
    cache_key: [u8; 32],
    digest: [u8; 32],
    prompt: String,
    source: crate::protocol::ir::MediaSource,
}

/// Apply the vision shim to one request. No-op (empty stats) unless the
/// matched model route enables the shim and the request carries images.
pub(crate) async fn process(
    gw: &Gateway,
    request: &mut AiRequest,
    deadline: &Deadline,
) -> Result<VisionShimStats, GatewayError> {
    let mut stats = VisionShimStats::default();

    // ── Resolve the route and its shim configuration ─────────────────────
    let model_name = request.model.clone();
    let route = {
        let model_cache = gw.model_cache.read().await;
        model_cache.match_model(&model_name).cloned()
    };
    let Some(route) = route else {
        // No route: the dispatcher will produce its own 404.
        return Ok(stats);
    };
    let Some(cfg) = route.vision_shim_config() else {
        // Vision shim not enabled for this model route.
        return Ok(stats);
    };
    if cfg
        .helper_model
        .trim()
        .eq_ignore_ascii_case(route.target_model.trim())
    {
        tracing::warn!(
            model = %route.name,
            helper = %cfg.helper_model,
            "vision shim disabled: helper model equals the target model"
        );
        return Ok(stats);
    }

    // ── Scan for images ───────────────────────────────────────────────────
    let images = ir_scan::collect_images(request);
    if images.is_empty() {
        return Ok(stats);
    }
    stats.images = images.len();
    stats.helper_model = cfg.helper_model.clone();
    let question = ir_scan::latest_user_text(request);
    let started = Instant::now();

    // ── Resolve the helper provider ───────────────────────────────────────
    let helper_provider_id = cfg
        .helper_provider
        .clone()
        .unwrap_or_else(|| route.target_provider.clone());
    let provider = gw
        .storage
        .providers()
        .get(&helper_provider_id)
        .await
        .ok()
        .flatten()
        .filter(|provider| provider.is_enabled)
        .filter(|provider| {
            provider
                .protocol
                .trim()
                .eq_ignore_ascii_case("openai-compatible")
        });

    let Some(provider) = provider else {
        let reason =
            format!("helper provider unavailable or not openai-compatible: {helper_provider_id}");
        tracing::warn!(model = %route.name, %reason, "vision shim cannot caption images");
        return match cfg.on_failure {
            VisionShimFailureMode::Reject => {
                Err(helper_unavailable_error(&helper_provider_id, &reason))
            }
            VisionShimFailureMode::Placeholder => {
                let mut captions = HashMap::new();
                for image in &images {
                    captions.insert(image.digest, format!("(vision shim unavailable: {reason})"));
                }
                stats.placeholders = images.len();
                stats.caption_latency_ms = started.elapsed().as_millis() as u64;
                ir_scan::replace_images(request, &captions);
                Ok(stats)
            }
        };
    };

    // ── Deduplicate by content digest and prepare caption jobs ───────────
    let mut captions: HashMap<[u8; 32], String> = HashMap::new();
    let mut jobs: Vec<CaptionJob> = Vec::new();

    for (idx, image) in images.iter().enumerate() {
        if captions.contains_key(&image.digest) {
            continue;
        }
        let limit_note = if idx >= cfg.max_images {
            Some("image skipped: per-request image limit reached".to_string())
        } else if matches!(
            image.source,
            crate::protocol::ir::MediaSource::FileId { .. }
        ) {
            Some("file-referenced images are not supported by the vision facade".to_string())
        } else if image.approx_bytes > cfg.max_image_bytes {
            Some("image exceeds the configured size limit".to_string())
        } else {
            None
        };
        if let Some(note) = limit_note {
            captions.insert(image.digest, format!("({note})"));
            stats.placeholders += 1;
            continue;
        }

        // Latest-user-message images get question-aware captions; history
        // images get the stable generic prompt so cache hits survive turns.
        let question_for_image = if image.question_aware {
            question.as_deref()
        } else {
            None
        };
        let prompt = cfg.caption_prompt(question_for_image);
        let cache_key = caption_cache_key(&prompt, &image.digest);
        if let Some(hit) = cache::lookup(&cache_key) {
            captions.insert(image.digest, hit);
            stats.cache_hits += 1;
            continue;
        }
        jobs.push(CaptionJob {
            cache_key,
            digest: image.digest,
            prompt,
            source: image.source.clone(),
        });
    }

    // ── Caption cache misses concurrently through the helper model ────────
    let timeout = deadline.remaining().min(Duration::from_secs(60));
    if !jobs.is_empty() && !timeout.is_zero() {
        let results = join_all(jobs.iter().map(|job| async {
            let call =
                caption::caption_image(gw, &provider, &cfg, &job.source, &job.prompt, timeout)
                    .await;
            (job.cache_key, job.digest, call)
        }))
        .await;

        for (cache_key, digest, call) in results {
            match call.text {
                Some(text) => {
                    cache::store(
                        cache_key,
                        text.clone(),
                        Duration::from_secs(cfg.cache_ttl_secs),
                    );
                    captions.insert(digest, text);
                    stats.captioned += 1;
                    stats.helper_tokens += call.usage_total_tokens.unwrap_or(0);
                }
                None => {
                    let error = call.error.unwrap_or_else(|| "caption failed".to_string());
                    tracing::warn!(
                        model = %route.name,
                        helper = %cfg.helper_model,
                        error = %error,
                        "vision shim caption failed"
                    );
                    match cfg.on_failure {
                        VisionShimFailureMode::Reject => {
                            return Err(GatewayError::UpstreamStatus {
                                provider: helper_provider_id,
                                status: 502,
                                body: Some(format!("vision shim helper failed: {error}")),
                            });
                        }
                        VisionShimFailureMode::Placeholder => {
                            captions.insert(
                                digest,
                                format!("(vision shim could not process this image: {error})"),
                            );
                            stats.placeholders += 1;
                        }
                    }
                }
            }
        }
    } else if !jobs.is_empty() {
        // Deadline exhausted: degrade per the configured failure mode.
        let note = "(vision shim skipped: request deadline exhausted)".to_string();
        for job in jobs {
            captions.insert(job.digest, note.clone());
            stats.placeholders += 1;
        }
    }

    // ── Rewrite the request ───────────────────────────────────────────────
    let replaced = ir_scan::replace_images(request, &captions);
    debug_assert_eq!(replaced, images.len());
    stats.caption_latency_ms = started.elapsed().as_millis() as u64;
    Ok(stats)
}

/// Caption cache key: prompt bytes (which embed the prompt version and, for
/// question-aware captions, the user's question) over the image digest.
fn caption_cache_key(prompt: &str, digest: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([CAPTION_PROMPT_VERSION]);
    hasher.update(prompt.as_bytes());
    hasher.update(digest);
    hasher.finalize().into()
}

fn helper_unavailable_error(provider_id: &str, reason: &str) -> GatewayError {
    GatewayError::UpstreamStatus {
        provider: provider_id.to_string(),
        status: 502,
        body: Some(format!("vision shim helper unavailable: {reason}")),
    }
}
