//! Encoding-local correlation. IDs remain IDs in the IR; only the Gemini
//! functionResponse.name is resolved to the preceding function's wire name.
use std::collections::HashMap;

use crate::error::GatewayError;
use crate::protocol::ir::{ContentBlock, Message, MessageContent, Role};
use anyhow::Result;

#[derive(Default)]
pub(super) struct ToolNames {
    calls: HashMap<String, String>,
    order: Vec<String>,
    consumed: std::collections::HashSet<String>,
}

impl ToolNames {
    pub(super) fn observe(&mut self, message: &Message, position: usize) -> Result<()> {
        if message.role != Role::Assistant {
            return Ok(());
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                self.insert(&call.id, &call.name, position)?;
            }
        }
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse { id, name, .. }
                    | ContentBlock::ServerToolUse { id, name, .. } => {
                        self.insert(id, name, position)?
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn insert(&mut self, id: &str, name: &str, position: usize) -> Result<()> {
        if id.is_empty() {
            return Ok(());
        }
        if name.trim().is_empty() {
            return Err(GatewayError::BadRequest {
                code: "gemini_tool_name_missing",
                msg: format!("messages[{position}]: tool call has an empty function name"),
            }
            .into());
        }
        let completed = self.consumed.remove(id);
        if !completed && self.calls.get(id).is_some_and(|previous| previous != name) {
            return Err(GatewayError::BadRequest {
                code: "gemini_tool_id_conflict",
                msg: format!(
                    "messages[{position}]: tool call ID maps to conflicting function names"
                ),
            }
            .into());
        }
        if !self.calls.contains_key(id) {
            self.order.push(id.to_owned());
        }
        self.calls.insert(id.to_owned(), name.to_owned());
        Ok(())
    }

    pub(super) fn resolve(
        &mut self,
        id: &str,
        position: usize,
        preview: bool,
        name_only: bool,
    ) -> Result<(String, String)> {
        if let Some(name) = self.calls.get(id) {
            self.consumed.insert(id.to_owned());
            return Ok((id.to_owned(), name.clone()));
        }
        // Only a legacy Gemini ingress may identify results by function name.
        // Match pending same-name calls in their original occurrence order,
        // just like the native request normalizer, not HashMap iteration order.
        // Opaque OpenAI/Anthropic IDs never fall back to guessed names.
        if let Some(key) = self
            .order
            .iter()
            .find(|key| {
                name_only
                    && self.calls.get(*key).is_some_and(|name| name == id)
                    && !self.consumed.contains(*key)
            })
            .cloned()
        {
            self.consumed.insert(key.clone());
            return Ok((key, id.to_owned()));
        }
        // Preview output is never selected as native wire; don't veto a valid
        // raw-wire conversion while computing vendor diffs.
        if preview {
            return Ok((id.to_owned(), id.to_owned()));
        }
        Err(GatewayError::BadRequest {
            code: "gemini_unmatched_tool_result",
            msg: format!("messages[{position}]: cannot resolve functionResponse.name from a preceding tool call"),
        }.into())
    }
}
