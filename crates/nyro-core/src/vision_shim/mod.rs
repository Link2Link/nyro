//! Vision Shim — a multimodal facade over text-only target models.
//!
//! Some providers pair a strong text-only model with a smaller multimodal
//! sibling (GLM's `glm-5.3` next to `glm-5.3-flash`, for example). The vision
//! shim lets clients treat the text-only model as multimodal: images in the
//! incoming request are transcribed into dense, question-aware text by the
//! helper model before the request is forwarded, and the image blocks are
//! replaced with `[Image n: …]` text blocks. The client never sees an
//! "unsupported image" error — to it, the target model simply understands
//! images.
//!
//! Mechanism (generic, per model route — reusable across vendors):
//!
//! - Configuration lives on the model row (`models.vision_shim`, JSON:
//!   `helper_backends` — provider+model pairs selected like target-model
//!   backends, spanning vendors with in-order failover; or the legacy single
//!   `helper_model`(+`helper_provider`) form — plus limits and failure
//!   policy) — see [`VisionShimConfig`].
//! - A [`PhaseHook`](crate::plugin::phase::PhaseHook) (`vision-shim`) runs in
//!   the `OnAccess` phase: route + auth resolved, before upstream work. It
//!   scans the unified IR for images (message blocks, tool results, nested
//!   search results), captions them through the helper provider's
//!   OpenAI-compatible endpoint, and rewrites the IR.
//! - Captions are cached by SHA-256(prompt ++ content) so multi-turn chats
//!   (which resend history images every turn) hit the cache; only images in
//!   the latest user message get question-aware captions, history images use
//!   the stable generic prompt.
//! - Failures degrade to placeholders by default (`on_failure: "reject"` to
//!   hard-fail instead) so the facade never leaks capability errors.
//!
//! The shim is wired through the generic phase-hook bus and self-registers
//! via `inventory`, so it shows up in the admin "loaded extensions" panel
//! automatically.

mod cache;
mod caption;
mod config;
mod hook;
mod ir_scan;

pub use config::{VisionShimConfig, VisionShimFailureMode};
pub use hook::apply;
pub use hook::{VisionShimHook, VisionShimStats};
