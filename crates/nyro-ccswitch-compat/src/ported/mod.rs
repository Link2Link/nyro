// Ported from cc-switch at eb69e4922ee187a261fd29c216a738e838f85bc4.
// Copyright (c) 2025 Jason Young. Licensed under MIT.
//
// The ported core intentionally mirrors the upstream source shape:
// - nested `if` statements stay expanded so diffs against cc-switch remain
//   line-comparable (clippy::collapsible_if);
// - several helpers are ported for completeness but not yet referenced by
//   the engine facade (dead_code).
#![allow(clippy::collapsible_if, dead_code)]

pub(crate) mod content_encoding;
pub(crate) mod error;
pub(crate) mod handlers_compat;
pub(crate) mod json_canonical;
pub(crate) mod providers;
pub(crate) mod sse;
pub(crate) mod thinking_optimizer;
pub(crate) mod tool_media;
pub(crate) mod usage;
