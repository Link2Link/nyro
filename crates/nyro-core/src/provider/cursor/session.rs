//! A single HTTP/2 Run's pure duplex state machine. No IO, tasks, global state,
//! retries, upstream credentials, or host execution live here.
use super::{
    catalog::CursorModelRoute,
    errors::CursorError,
    framing::{self, ConnectDecoder, Frame, MAX_INFLATED_FRAME},
    history::{self, BlobStore},
    wire,
};
use crate::protocol::ir::{AiRequest, AiStreamDelta, ToolCall, ToolCallKind, Usage};
use prost::Message as _;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    /// turnEnded plus a clean Connect/HTTP terminal, checked at response EOF.
    Completed,
    /// Local deliberate cancellation for a caller-owned tool, NOT turnEnded.
    ToolHandoff,
}
#[derive(Debug)]
pub enum Action {
    SendFrame(Vec<u8>),
    Event(AiStreamDelta),
    Terminal(TerminalReason),
    /// Send preceding rejection frames before surfacing this failure.
    Failure(CursorError),
}
/// Unknown counters stay unknown. Only a complete authoritative input/output
/// pair can also be represented by the older integer-valued IR Usage type.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CursorUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
}
#[derive(Debug)]
struct PendingTool {
    envelope_id: String,
    id: String,
    name: String,
    partial: String,
}
#[derive(Debug)]
pub struct CursorSession {
    decoder: ConnectDecoder,
    blobs: BlobStore,
    context: wire::RequestContext,
    tools: HashSet<String>,
    pending: Option<PendingTool>,
    require_tool: bool,
    native_notification_seen: bool,
    turn_ended: bool,
    clean_trailers: bool,
    terminal: Option<TerminalReason>,
    failed: bool,
    usage: CursorUsage,
}
impl CursorSession {
    pub fn start(
        request: &AiRequest,
        route: CursorModelRoute,
    ) -> Result<(Self, Vec<Action>), CursorError> {
        if route.legacy_model_id.is_empty() || route.requested_model_id.is_empty() {
            return Err(CursorError::InvalidRequest("empty model route".into()));
        }
        let built = history::build_history(request)?;
        let conversation_id = uuid::Uuid::new_v4().to_string();
        let run = wire::AgentRunRequest {
            conversation_state: Some(built.state),
            action: Some(built.action),
            model_details: Some(wire::ModelDetails {
                model_id: route.legacy_model_id,
                display_model_id: request.model.clone(),
                max_mode: route.max_mode,
                ..Default::default()
            }),
            requested_model: Some(wire::RequestedModel {
                model_id: route.requested_model_id,
                max_mode: route.max_mode.unwrap_or(false),
                parameters: route.parameters,
            }),
            conversation_id: Some(conversation_id.clone()),
        };
        let actions = vec![
            Action::Event(AiStreamDelta::MessageStart {
                id: conversation_id,
                model: request.model.clone(),
            }),
            send(wire::agent_client_message::Message::RunRequest(run))?,
        ];
        Ok((
            Self {
                decoder: ConnectDecoder::new(),
                blobs: built.blobs,
                context: built.context,
                tools: built.tools,
                pending: None,
                require_tool: matches!(
                    request.tool_choice,
                    Some(
                        crate::protocol::ir::ToolChoice::Required
                            | crate::protocol::ir::ToolChoice::Named { .. }
                    )
                ),
                native_notification_seen: false,
                turn_ended: false,
                clean_trailers: false,
                terminal: None,
                failed: false,
                usage: CursorUsage::default(),
            },
            actions,
        ))
    }
    pub fn usage(&self) -> &CursorUsage {
        &self.usage
    }
    pub fn terminal_reason(&self) -> Option<TerminalReason> {
        self.terminal
    }
    /// A validated transport end allows the client to half-close its upload,
    /// while still draining HTTP trailers/EOF before publishing completion.
    pub fn upload_finished(&self) -> bool {
        self.decoder.saw_end() || self.clean_trailers || self.terminal.is_some()
    }
    pub fn heartbeat_frame(&self) -> Result<Vec<u8>, CursorError> {
        self.ensure_open()?;
        framing::encode_frame(
            &wire::AgentClientMessage {
                message: Some(wire::agent_client_message::Message::Heartbeat(
                    wire::Empty {},
                )),
            }
            .encode_to_vec(),
        )
    }
    pub fn accept_data(&mut self, data: &[u8]) -> Result<Vec<Action>, CursorError> {
        self.ensure_open()?;
        let result = self.accept_data_inner(data);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn accept_data_inner(&mut self, data: &[u8]) -> Result<Vec<Action>, CursorError> {
        let frames = self.decoder.push(data)?;
        let mut actions = Vec::new();
        for frame in frames {
            if self.terminal == Some(TerminalReason::ToolHandoff) || self.failed {
                break;
            }
            if let Frame::Data(bytes) = frame {
                let message = wire::AgentServerMessage::decode(bytes.as_slice())?
                    .message
                    .ok_or_else(|| {
                        CursorError::Unsupported(
                            "unknown/empty AgentServerMessage operation".into(),
                        )
                    })?;
                self.handle(message, &mut actions)?;
            }
        }
        Ok(actions)
    }
    pub fn accept_trailers<'a>(
        &mut self,
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Vec<Action>, CursorError> {
        self.ensure_open()?;
        match framing::validate_trailers(headers) {
            Ok(clean) => {
                self.clean_trailers |= clean;
                Ok(vec![])
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    /// Call only when the HTTP response has reached EOF and trailers were read.
    /// A turnEnded alone, an empty HTTP body, or a bare EOF is not success.
    pub fn finish(&mut self) -> Result<Vec<Action>, CursorError> {
        self.ensure_open()?;
        if self.decoder.finish().is_err()
            || !self.turn_ended
            || !(self.decoder.saw_end() || self.clean_trailers)
            || self.pending.is_some()
        {
            self.failed = true;
            return Err(CursorError::UnexpectedEof);
        }
        self.terminal = Some(TerminalReason::Completed);
        let mut actions = Vec::new();
        if let (Some(input), Some(output)) = (self.usage.input_tokens, self.usage.output_tokens) {
            let total = input
                .checked_add(output)
                .ok_or_else(|| CursorError::Protocol("usage counter overflow".into()))?;
            actions.push(Action::Event(AiStreamDelta::Usage(Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                cache_read_tokens: self.usage.cache_read_tokens,
                cache_creation_tokens: self.usage.cache_write_tokens,
                reasoning_tokens: self.usage.reasoning_tokens,
                server_tool_use: None,
            })));
        }
        actions.push(Action::Event(AiStreamDelta::Done {
            stop_reason: "stop".into(),
        }));
        actions.push(Action::Terminal(TerminalReason::Completed));
        Ok(actions)
    }
    fn ensure_open(&self) -> Result<(), CursorError> {
        if self.failed || self.terminal.is_some() {
            Err(CursorError::AlreadyTerminated)
        } else {
            Ok(())
        }
    }
    fn handle(
        &mut self,
        message: wire::agent_server_message::Message,
        actions: &mut Vec<Action>,
    ) -> Result<(), CursorError> {
        use wire::agent_server_message::Message;
        match message {
            Message::Kv(kv) => {
                let result = match kv
                    .message
                    .ok_or_else(|| CursorError::Unsupported("unknown KV operation".into()))?
                {
                    wire::kv_server_message::Message::Get(args) => {
                        wire::kv_client_message::Message::Get(wire::GetBlobResult {
                            blob_data: Some(self.blobs.get(&args.blob_id)?.to_vec()),
                        })
                    }
                    wire::kv_server_message::Message::Set(args) => {
                        self.blobs.set(args.blob_id, args.blob_data)?;
                        wire::kv_client_message::Message::Set(wire::Empty {})
                    }
                };
                actions.push(send(wire::agent_client_message::Message::Kv(
                    wire::KvClientMessage {
                        id: kv.id,
                        message: Some(result),
                    },
                ))?);
            }
            // Checkpoint metadata is intentionally not reused across HTTP calls.
            Message::Checkpoint(_) => {}
            Message::Interaction(update) => self.interaction(update, actions)?,
            Message::Exec(exec) => self.exec(exec, actions)?,
            Message::ExecControl(_) => {
                return Err(CursorError::Unsupported("exec server control".into()));
            }
            Message::InteractionQuery(_) => {
                return Err(CursorError::Unsupported(
                    "interactive approval/query".into(),
                ));
            }
        }
        Ok(())
    }
    fn interaction(
        &mut self,
        update: wire::InteractionUpdate,
        actions: &mut Vec<Action>,
    ) -> Result<(), CursorError> {
        use wire::interaction_update::Message;
        let message = update
            .message
            .ok_or_else(|| CursorError::Unsupported("unknown interaction update".into()))?;
        if self.turn_ended && !matches!(message, Message::Heartbeat(_)) {
            return Err(CursorError::Protocol("interaction after turnEnded".into()));
        }
        match message {
            Message::Text(text) => actions.push(Action::Event(AiStreamDelta::TextDelta(text.text))),
            Message::Thinking(text) => {
                actions.push(Action::Event(AiStreamDelta::ThinkingDelta(text.text)))
            }
            Message::ToolStarted(call) => {
                let tool = call
                    .tool_call
                    .ok_or_else(|| CursorError::Protocol("tool start missing tool".into()))?;
                // Native notifications are informational. Wait for the matching
                // exec to send an explicit rejection; never execute or emit them.
                if tool.mcp_tool_call.is_none() {
                    self.native_notification_seen = true;
                    return Ok(());
                }
                let args = mcp_args(tool, &call.call_id)?;
                self.begin_tool(&args, &call.call_id, actions)?;
            }
            Message::PartialTool(call) => {
                if let Some(tool) = call.tool_call {
                    let args = mcp_args(tool, &call.call_id)?;
                    self.begin_tool(&args, &call.call_id, actions)?;
                }
                let pending = self.pending.as_mut().ok_or_else(|| {
                    CursorError::Protocol("partial tool without MCP start".into())
                })?;
                if !matches_pending(pending, &call.call_id) {
                    return Err(CursorError::Unsupported("parallel tool calls".into()));
                }
                let fragment = if call.args_text_delta.starts_with(&pending.partial) {
                    &call.args_text_delta[pending.partial.len()..]
                } else {
                    &call.args_text_delta
                };
                if pending.partial.len().saturating_add(fragment.len()) > MAX_INFLATED_FRAME {
                    return Err(CursorError::LimitExceeded {
                        resource: "tool arguments",
                        limit: MAX_INFLATED_FRAME,
                    });
                }
                // Buffer snapshots until the authoritative exec args arrive.
                // This avoids emitting contradictory/replaced JSON to callers.
                pending.partial.push_str(fragment);
            }
            Message::ToolCompleted(call) => {
                let tool = call
                    .tool_call
                    .ok_or_else(|| CursorError::Protocol("tool completion missing tool".into()))?;
                if tool
                    .mcp_tool_call
                    .as_ref()
                    .is_some_and(|m| m.result.is_some())
                {
                    return Err(CursorError::Unsupported(
                        "server-executed caller MCP tool".into(),
                    ));
                }
                let args = mcp_args(tool, &call.call_id)?;
                self.handoff(args, &call.call_id, actions)?;
            }
            Message::TurnEnded(usage) => {
                if self.require_tool {
                    return Err(CursorError::Protocol(
                        "required tool choice was not fulfilled".into(),
                    ));
                }
                if self.native_notification_seen {
                    return Err(CursorError::Unsupported(
                        "native tool notification without supported execution".into(),
                    ));
                }
                if self.pending.is_some() {
                    return Err(CursorError::Protocol(
                        "turnEnded with incomplete tool call".into(),
                    ));
                }
                self.usage = CursorUsage {
                    input_tokens: counter(usage.input_tokens)?,
                    output_tokens: counter(usage.output_tokens)?,
                    cache_read_tokens: counter(usage.cache_read_tokens)?,
                    cache_write_tokens: counter(usage.cache_write_tokens)?,
                    reasoning_tokens: counter(usage.reasoning_tokens)?,
                };
                self.turn_ended = true;
            }
            Message::Heartbeat(_)
            | Message::ThinkingCompleted(_)
            | Message::UserAppended(_)
            | Message::StepStarted(_)
            | Message::StepCompleted(_)
            | Message::TokenDelta(_) => {}
            Message::Summary(_) | Message::SummaryStarted(_) | Message::SummaryCompleted(_) => {
                return Err(CursorError::Unsupported(
                    "server conversation summarization".into(),
                ));
            }
            Message::ShellOutput(_) | Message::ToolDelta(_) => {
                return Err(CursorError::Unsupported(
                    "native shell/edit/task streaming".into(),
                ));
            }
        }
        Ok(())
    }
    fn begin_tool(
        &mut self,
        args: &wire::McpArgs,
        envelope: &str,
        actions: &mut Vec<Action>,
    ) -> Result<(), CursorError> {
        let name = tool_name(args);
        if !self.tools.contains(name) {
            return Err(CursorError::InvalidRequest(format!(
                "upstream requested undeclared tool {name:?}"
            )));
        }
        if args.tool_call_id.is_empty() {
            return Err(CursorError::Protocol("MCP call missing ID".into()));
        }
        if args.smart_mode_approval_only {
            return Err(CursorError::Unsupported("MCP approval-only request".into()));
        }
        if let Some(pending) = &self.pending {
            if pending.id != args.tool_call_id || pending.name != name {
                return Err(CursorError::Unsupported(
                    "parallel or changed tool calls".into(),
                ));
            }
            if !envelope.is_empty() && !matches_pending(pending, envelope) {
                return Err(CursorError::Protocol("MCP envelope ID changed".into()));
            }
            return Ok(());
        }
        self.pending = Some(PendingTool {
            envelope_id: envelope.into(),
            id: args.tool_call_id.clone(),
            name: name.into(),
            partial: String::new(),
        });
        actions.push(Action::Event(AiStreamDelta::ToolCallStart {
            index: 0,
            id: args.tool_call_id.clone(),
            name: name.into(),
            namespace: None,
            kind: ToolCallKind::Function,
        }));
        Ok(())
    }
    fn handoff(
        &mut self,
        args: wire::McpArgs,
        envelope: &str,
        actions: &mut Vec<Action>,
    ) -> Result<(), CursorError> {
        self.begin_tool(&args, envelope, actions)?;
        let arguments = history::decode_arguments(&args.args)?;
        let pending = self.pending.take().unwrap();
        if !pending.partial.is_empty() {
            match serde_json::from_str::<Value>(&pending.partial) {
                Ok(partial) if partial != arguments => {
                    return Err(CursorError::Protocol(
                        "MCP argument snapshot differs from final args".into(),
                    ));
                }
                // The last preview may remain incomplete. Only final protobuf
                // arguments, never the preview, are exposed to the caller.
                Err(error) if !error.is_eof() => {
                    return Err(CursorError::Protocol(
                        "invalid MCP argument snapshot".into(),
                    ));
                }
                _ => {}
            }
        }
        let arguments = serde_json::to_string(&arguments)?;
        actions.push(Action::Event(AiStreamDelta::ToolCallDelta {
            index: 0,
            arguments: arguments.clone(),
        }));
        actions.push(Action::Event(AiStreamDelta::ToolCallComplete {
            index: 0,
            tool_call: ToolCall::function(pending.id, pending.name, arguments),
        }));
        actions.push(Action::Event(AiStreamDelta::Done {
            stop_reason: "tool_use".into(),
        }));
        self.terminal = Some(TerminalReason::ToolHandoff);
        actions.push(Action::Terminal(TerminalReason::ToolHandoff));
        Ok(())
    }
    fn exec(
        &mut self,
        exec: wire::ExecServerMessage,
        actions: &mut Vec<Action>,
    ) -> Result<(), CursorError> {
        use wire::exec_server_message::Message;
        if self.turn_ended {
            return Err(CursorError::Protocol("exec after turnEnded".into()));
        }
        let message = exec
            .message
            .ok_or_else(|| CursorError::Unsupported("unknown exec operation".into()))?;
        match message {
            Message::RequestContext(_) => {
                actions.push(send(wire::agent_client_message::Message::Exec(
                    wire::ExecClientMessage {
                        id: exec.id,
                        exec_id: exec.exec_id,
                        request_context_result: Some(wire::RequestContextResult {
                            success: Some(wire::RequestContextSuccess {
                                request_context: Some(self.context.clone()),
                            }),
                        }),
                        ..Default::default()
                    },
                ))?);
            }
            Message::Mcp(args) => self.handoff(args, "", actions)?,
            native => {
                let reason =
                    "Nyro does not execute Cursor native tools; use declared caller MCP tools";
                let mut result = wire::ExecClientMessage {
                    id: exec.id,
                    exec_id: exec.exec_id,
                    ..Default::default()
                };
                let has_typed_result = match &native {
                    Message::Shell(bytes) | Message::ShellStream(bytes) => {
                        let args = wire::ShellArgs::decode(bytes.as_slice())?;
                        result.shell_result = Some(wire::ShellRejectResult {
                            rejected: Some(wire::ShellRejected {
                                command: args.command,
                                working_directory: args.working_directory,
                                reason: reason.into(),
                            }),
                        });
                        true
                    }
                    Message::Write(bytes)
                    | Message::Delete(bytes)
                    | Message::Read(bytes)
                    | Message::Ls(bytes) => {
                        let path = wire::PathArgs::decode(bytes.as_slice())?.path;
                        let rejected = Some(wire::PathRejected {
                            path,
                            reason: reason.into(),
                        });
                        match native {
                            Message::Write(_) => {
                                result.write_result =
                                    Some(wire::WriteDeleteRejectResult { rejected })
                            }
                            Message::Delete(_) => {
                                result.delete_result =
                                    Some(wire::WriteDeleteRejectResult { rejected })
                            }
                            Message::Read(_) => {
                                result.read_result = Some(wire::ReadLsRejectResult { rejected })
                            }
                            Message::Ls(_) => {
                                result.ls_result = Some(wire::ReadLsRejectResult { rejected })
                            }
                            _ => unreachable!(),
                        }
                        true
                    }
                    Message::Grep(_) => {
                        result.grep_result = Some(wire::GrepErrorResult {
                            error: Some(wire::ErrorText {
                                error: reason.into(),
                            }),
                        });
                        true
                    }
                    _ => false,
                };
                if has_typed_result {
                    actions.push(send(wire::agent_client_message::Message::Exec(result))?);
                } else {
                    actions.push(send(wire::agent_client_message::Message::ExecControl(
                        wire::ExecClientControlMessage {
                            message: Some(wire::exec_client_control_message::Message::Throw(
                                wire::ExecClientThrow {
                                    id: exec.id,
                                    error: reason.into(),
                                    error_code: Some("NYRO_UNSUPPORTED_NATIVE_EXEC".into()),
                                },
                            )),
                        },
                    ))?);
                }
                actions.push(send(wire::agent_client_message::Message::ExecControl(
                    wire::ExecClientControlMessage {
                        message: Some(wire::exec_client_control_message::Message::Close(
                            wire::ExecClientStreamClose { id: exec.id },
                        )),
                    },
                ))?);
                self.failed = true;
                actions.push(Action::Failure(CursorError::Unsupported(
                    "native execution rejected".into(),
                )));
            }
        }
        Ok(())
    }
}
fn tool_name(args: &wire::McpArgs) -> &str {
    if args.tool_name.is_empty() {
        &args.name
    } else {
        &args.tool_name
    }
}
fn matches_pending(pending: &PendingTool, id: &str) -> bool {
    pending.envelope_id == id || pending.id == id
}
fn mcp_args(tool: wire::ToolCall, envelope: &str) -> Result<wire::McpArgs, CursorError> {
    let mut args = tool
        .mcp_tool_call
        .and_then(|m| m.args)
        .ok_or_else(|| CursorError::Unsupported("non-MCP or incomplete tool call".into()))?;
    if args.tool_call_id.is_empty() {
        args.tool_call_id = tool.tool_call_id.unwrap_or_else(|| envelope.into());
    }
    Ok(args)
}
fn counter(n: Option<i64>) -> Result<Option<u32>, CursorError> {
    n.map(|n| {
        u32::try_from(n)
            .map_err(|_| CursorError::Protocol("negative or overflowing usage counter".into()))
    })
    .transpose()
}
fn send(message: wire::agent_client_message::Message) -> Result<Action, CursorError> {
    Ok(Action::SendFrame(framing::encode_frame(
        &wire::AgentClientMessage {
            message: Some(message),
        }
        .encode_to_vec(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ir::{Message, MessageContent, Role, ToolSpec};
    use serde_json::json;
    fn request() -> AiRequest {
        let mut req = AiRequest::new(
            "Exact-Model",
            vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                tool_calls: None,
                tool_call_id: None,
                meta: None,
            }],
        );
        req.tools = Some(vec![ToolSpec {
            name: "lookup".into(),
            namespace: None,
            description: Some("Lookup".into()),
            kind: Default::default(),
            parameters: json!({"type":"object","properties":{"q":{"type":"string"}}}),
            strict: None,
            cache_control: None,
            meta: None,
        }]);
        req
    }
    fn session() -> CursorSession {
        CursorSession::start(&request(), CursorModelRoute::exact("Exact-Model"))
            .unwrap()
            .0
    }
    fn frame(message: wire::agent_server_message::Message) -> Vec<u8> {
        framing::encode_frame(
            &wire::AgentServerMessage {
                message: Some(message),
            }
            .encode_to_vec(),
        )
        .unwrap()
    }
    fn update(message: wire::interaction_update::Message) -> Vec<u8> {
        frame(wire::agent_server_message::Message::Interaction(
            wire::InteractionUpdate {
                message: Some(message),
            },
        ))
    }
    fn end() -> Vec<u8> {
        vec![2, 0, 0, 0, 2, b'{', b'}']
    }
    fn mcp() -> wire::McpArgs {
        wire::McpArgs {
            name: "lookup".into(),
            tool_name: "lookup".into(),
            tool_call_id: "tool-1".into(),
            args: history::encode_arguments(&json!({"q":"test"})).unwrap(),
            ..Default::default()
        }
    }
    #[test]
    fn complete_after_turn_end_connect_end_and_eof_not_before() {
        let mut s = session();
        let stream = [
            update(wire::interaction_update::Message::Text(wire::TextMessage {
                text: "hi".into(),
            })),
            update(wire::interaction_update::Message::Thinking(
                wire::TextMessage {
                    text: "reason".into(),
                },
            )),
            update(wire::interaction_update::Message::TurnEnded(
                wire::TurnEndedUpdate::default(),
            )),
            end(),
        ]
        .concat();
        let actions = s.accept_data(&stream).unwrap();
        assert!(matches!(
            actions[0],
            Action::Event(AiStreamDelta::TextDelta(_))
        ));
        assert!(matches!(
            actions[1],
            Action::Event(AiStreamDelta::ThinkingDelta(_))
        ));
        assert!(actions.iter().all(|a| !matches!(a, Action::Terminal(_))));
        let done = s.finish().unwrap();
        assert!(matches!(
            done.last(),
            Some(Action::Terminal(TerminalReason::Completed))
        ));
        assert!(
            !done
                .iter()
                .any(|a| matches!(a, Action::Event(AiStreamDelta::Usage(_))))
        );
        assert_eq!(s.usage(), &CursorUsage::default());
    }
    #[test]
    fn late_trailer_error_never_emits_success() {
        let mut s = session();
        s.accept_data(
            &[
                update(wire::interaction_update::Message::TurnEnded(
                    Default::default(),
                )),
                end(),
            ]
            .concat(),
        )
        .unwrap();
        assert!(matches!(
            s.accept_trailers([("grpc-status", "13")]),
            Err(CursorError::Upstream { .. })
        ));
        assert!(s.finish().is_err());
    }
    #[test]
    fn eof_and_step_completed_not_success() {
        let mut s = session();
        s.accept_data(&update(wire::interaction_update::Message::StepCompleted(
            wire::Empty {},
        )))
        .unwrap();
        assert!(s.finish().is_err());
        let mut s = session();
        s.accept_data(&update(wire::interaction_update::Message::TurnEnded(
            Default::default(),
        )))
        .unwrap();
        assert!(s.finish().is_err());
        let mut s = session();
        s.accept_data(&end()).unwrap();
        assert!(s.finish().is_err());
    }
    #[test]
    fn authoritative_usage_only_and_grpc_terminal() {
        let mut s = session();
        s.accept_data(&update(wire::interaction_update::Message::TurnEnded(
            wire::TurnEndedUpdate {
                input_tokens: Some(100),
                output_tokens: Some(5),
                cache_read_tokens: Some(60),
                ..Default::default()
            },
        )))
        .unwrap();
        s.accept_trailers([("grpc-status", "0")]).unwrap();
        let done = s.finish().unwrap();
        let Action::Event(AiStreamDelta::Usage(u)) = &done[0] else {
            panic!()
        };
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 5);
        assert_eq!(u.total_tokens, 105);
    }
    #[test]
    fn mcp_first_handoff_emits_three_ir_tool_events_and_terminates() {
        let mut s = session();
        let actions = s
            .accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 9,
                    exec_id: "e".into(),
                    message: Some(wire::exec_server_message::Message::Mcp(mcp())),
                },
            )))
            .unwrap();
        assert!(matches!(
            actions[0],
            Action::Event(AiStreamDelta::ToolCallStart { .. })
        ));
        assert!(matches!(
            actions[1],
            Action::Event(AiStreamDelta::ToolCallDelta { .. })
        ));
        assert!(matches!(
            actions[2],
            Action::Event(AiStreamDelta::ToolCallComplete { .. })
        ));
        assert!(matches!(
            actions[4],
            Action::Terminal(TerminalReason::ToolHandoff)
        ));
        assert!(s.finish().is_err());
        assert!(s.accept_data(&[]).is_err());
    }
    #[test]
    fn partial_snapshots_validate_against_authoritative_args() {
        let mut s = session();
        let tool = wire::ToolCall {
            mcp_tool_call: Some(wire::McpToolCall {
                args: Some(mcp()),
                result: None,
            }),
            tool_call_id: None,
        };
        s.accept_data(&update(wire::interaction_update::Message::ToolStarted(
            wire::ToolCallUpdate {
                call_id: "envelope".into(),
                tool_call: Some(tool),
            },
        )))
        .unwrap();
        for snapshot in [r#"{"q":"#, r#"{"q":"test"}"#] {
            s.accept_data(&update(wire::interaction_update::Message::PartialTool(
                wire::PartialToolCallUpdate {
                    call_id: "envelope".into(),
                    tool_call: None,
                    args_text_delta: snapshot.into(),
                },
            )))
            .unwrap();
        }
        assert!(
            s.accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 1,
                    exec_id: "e".into(),
                    message: Some(wire::exec_server_message::Message::Mcp(mcp()))
                }
            )))
            .is_ok()
        );
    }
    #[test]
    fn incomplete_preview_accepts_final_but_invalid_preview_fails() {
        for (preview, accepted) in [(r#"{"q":"tes"#, true), ("{not-json}", false)] {
            let mut s = session();
            let call = wire::ToolCall {
                mcp_tool_call: Some(wire::McpToolCall {
                    args: Some(mcp()),
                    result: None,
                }),
                tool_call_id: None,
            };
            s.accept_data(&update(wire::interaction_update::Message::PartialTool(
                wire::PartialToolCallUpdate {
                    call_id: "preview".into(),
                    tool_call: Some(call),
                    args_text_delta: preview.into(),
                },
            )))
            .unwrap();
            let result = s.accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 1,
                    exec_id: String::new(),
                    message: Some(wire::exec_server_message::Message::Mcp(mcp())),
                },
            )));
            assert_eq!(result.is_ok(), accepted);
        }
    }
    #[test]
    fn undeclared_and_malformed_tools_fail() {
        for malformed in [false, true] {
            let mut args = mcp();
            if malformed {
                args.args.insert("bad".into(), vec![0xff]);
            } else {
                args.tool_name = "unknown".into();
            }
            let mut s = session();
            assert!(
                s.accept_data(&frame(wire::agent_server_message::Message::Exec(
                    wire::ExecServerMessage {
                        id: 1,
                        exec_id: "e".into(),
                        message: Some(wire::exec_server_message::Message::Mcp(args))
                    }
                )))
                .is_err()
            );
        }
    }
    #[test]
    fn request_context_advertises_value_schema_and_no_native_environment() {
        let mut s = session();
        let actions = s
            .accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 7,
                    exec_id: "context".into(),
                    message: Some(wire::exec_server_message::Message::RequestContext(
                        wire::Empty {},
                    )),
                },
            )))
            .unwrap();
        let Action::SendFrame(bytes) = &actions[0] else {
            panic!()
        };
        let wire::agent_client_message::Message::Exec(exec) =
            wire::AgentClientMessage::decode(&bytes[5..])
                .unwrap()
                .message
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(exec.id, 7);
        let context = exec
            .request_context_result
            .unwrap()
            .success
            .unwrap()
            .request_context
            .unwrap();
        let schema = prost_types::Value::decode(context.tools[0].input_schema.as_slice()).unwrap();
        assert_eq!(history::proto_to_json(schema).unwrap()["type"], "object");
        assert_eq!(context.web_search_enabled, Some(false));
    }
    #[test]
    fn native_exec_rejected_before_explicit_failure() {
        let mut s = session();
        let actions = s
            .accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 3,
                    exec_id: "native".into(),
                    message: Some(wire::exec_server_message::Message::Shell(
                        wire::ShellArgs {
                            command: "never run".into(),
                            working_directory: "/".into(),
                        }
                        .encode_to_vec(),
                    )),
                },
            )))
            .unwrap();
        let Action::SendFrame(bytes) = &actions[0] else {
            panic!()
        };
        let wire::agent_client_message::Message::Exec(exec) =
            wire::AgentClientMessage::decode(&bytes[5..])
                .unwrap()
                .message
                .unwrap()
        else {
            panic!()
        };
        assert!(exec.shell_result.unwrap().rejected.is_some());
        assert!(matches!(
            actions.last(),
            Some(Action::Failure(CursorError::Unsupported(_)))
        ));
        assert!(s.finish().is_err());
    }
    #[test]
    fn kv_reply_hash_validation_and_request_isolation() {
        let mut s = session();
        let blob = b"opaque blob".to_vec();
        use sha2::{Digest, Sha256};
        let id = Sha256::digest(&blob).to_vec();
        let set = frame(wire::agent_server_message::Message::Kv(
            wire::KvServerMessage {
                id: 2,
                message: Some(wire::kv_server_message::Message::Set(wire::SetBlobArgs {
                    blob_id: id.clone(),
                    blob_data: blob.clone(),
                })),
            },
        ));
        s.accept_data(&set).unwrap();
        let get = frame(wire::agent_server_message::Message::Kv(
            wire::KvServerMessage {
                id: 3,
                message: Some(wire::kv_server_message::Message::Get(wire::GetBlobArgs {
                    blob_id: id,
                })),
            },
        ));
        let actions = s.accept_data(&get).unwrap();
        let Action::SendFrame(bytes) = &actions[0] else {
            panic!()
        };
        let wire::agent_client_message::Message::Kv(kv) =
            wire::AgentClientMessage::decode(&bytes[5..])
                .unwrap()
                .message
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(kv.id, 3);
        let Some(wire::kv_client_message::Message::Get(result)) = kv.message else {
            panic!()
        };
        assert_eq!(result.blob_data, Some(blob));
        assert!(matches!(
            session().accept_data(&get),
            Err(CursorError::MissingBlob)
        ));
    }
    #[test]
    fn required_and_named_choices_are_enforced_without_fabricated_calls() {
        use crate::protocol::ir::ToolChoice;
        for choice in [
            ToolChoice::Required,
            ToolChoice::Named {
                name: "lookup".into(),
                namespace: None,
            },
        ] {
            let mut req = request();
            req.tool_choice = Some(choice);
            let mut s = CursorSession::start(&req, CursorModelRoute::exact("Exact-Model"))
                .unwrap()
                .0;
            assert!(
                s.accept_data(&update(wire::interaction_update::Message::TurnEnded(
                    Default::default()
                )))
                .is_err()
            );
        }
        let mut req = request();
        req.tool_choice = Some(ToolChoice::None);
        let mut s = CursorSession::start(&req, CursorModelRoute::exact("Exact-Model"))
            .unwrap()
            .0;
        assert!(
            s.accept_data(&frame(wire::agent_server_message::Message::Exec(
                wire::ExecServerMessage {
                    id: 1,
                    exec_id: String::new(),
                    message: Some(wire::exec_server_message::Message::Mcp(mcp()))
                }
            )))
            .is_err()
        );
    }
    #[test]
    fn golden_text_frame_and_split_session_stream() {
        // AgentServerMessage.interaction_update=1 / text_delta=1 / text=1.
        let golden = [0, 0, 0, 0, 7, 0x0a, 5, 0x0a, 3, 0x0a, 1, b'x'];
        let mut s = session();
        let mut actions = Vec::new();
        for byte in golden {
            actions.extend(s.accept_data(&[byte]).unwrap());
        }
        assert!(matches!(&actions[0], Action::Event(AiStreamDelta::TextDelta(t)) if t == "x"));
        // AgentClientMessage.kv=3 / id=1 / getBlobResult=2 / blobData=1.
        let reply = wire::AgentClientMessage {
            message: Some(wire::agent_client_message::Message::Kv(
                wire::KvClientMessage {
                    id: 1,
                    message: Some(wire::kv_client_message::Message::Get(wire::GetBlobResult {
                        blob_data: Some(vec![]),
                    })),
                },
            )),
        }
        .encode_to_vec();
        assert_eq!(reply, vec![0x1a, 6, 0x08, 1, 0x12, 2, 0x0a, 0]);
    }
    #[test]
    fn unknown_operation_and_malformed_protobuf_fail() {
        assert!(
            session()
                .accept_data(&framing::encode_frame(&[0x32, 0]).unwrap())
                .is_err()
        );
        assert!(
            session()
                .accept_data(&framing::encode_frame(&[0xff]).unwrap())
                .is_err()
        );
    }
}
