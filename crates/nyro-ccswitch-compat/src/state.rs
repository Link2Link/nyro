use std::sync::Arc;

use crate::ported::providers::codex_chat_history::CodexChatHistoryStore;
use crate::ported::providers::gemini_shadow::GeminiShadowStore;

/// Bounded in-memory compatibility state. No persistence or routing state lives
/// here.
#[derive(Debug, Clone)]
pub struct CompatState {
    pub(crate) gemini_shadow: Arc<GeminiShadowStore>,
    pub(crate) codex_chat_history: Arc<CodexChatHistoryStore>,
}

impl Default for CompatState {
    fn default() -> Self {
        Self {
            gemini_shadow: Arc::new(GeminiShadowStore::with_limits(200, 64)),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
        }
    }
}

impl CompatState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn gemini_session_count(&self) -> usize {
        self.gemini_shadow.session_count()
    }

    pub async fn codex_history_response_count(&self) -> usize {
        self.codex_chat_history.committed_response_count().await
    }
}
