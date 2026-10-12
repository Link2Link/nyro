//! Handwritten minimal prost schema; no protoc, CLI, or Node runtime dependency.
//! Source: can1357/oh-my-pi @ 5d0aaffaf1f987503ce129a642fb6e30339615af,
//! packages/ai/src/providers/cursor/proto/agent.proto and
//! packages/catalog/src/discovery/cursor-proto.ts (MIT; see PROTOCOL_SOURCES.md).
//! Omitted scalar/metadata fields are protobuf-forward-compatible. Unknown
//! operation oneofs must be rejected by session, never mistaken for success.
use prost::Message;
use std::collections::BTreeMap;

pub const SOURCE_REVISION: &str = "5d0aaffaf1f987503ce129a642fb6e30339615af";
pub const BASE_URL: &str = "https://api2.cursor.sh";
pub const RUN_PATH: &str = "/agent.v1.AgentService/Run";
pub const GET_USABLE_MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";
pub const CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
pub const MCP_PROVIDER: &str = "nyro-caller";

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}
#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1,2,3,5,7")]
    pub message: Option<agent_client_message::Message>,
}
pub mod agent_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::AgentRunRequest),
        #[prost(message, tag = "2")]
        Exec(super::ExecClientMessage),
        #[prost(message, tag = "3")]
        Kv(super::KvClientMessage),
        #[prost(message, tag = "5")]
        ExecControl(super::ExecClientControlMessage),
        #[prost(message, tag = "7")]
        Heartbeat(super::Empty),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1,2,3,4,5,7")]
    pub message: Option<agent_server_message::Message>,
}
pub mod agent_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Interaction(super::InteractionUpdate),
        #[prost(message, tag = "2")]
        Exec(super::ExecServerMessage),
        #[prost(message, tag = "3")]
        Checkpoint(super::ConversationStateStructure),
        #[prost(message, tag = "4")]
        Kv(super::KvServerMessage),
        #[prost(bytes, tag = "5")]
        ExecControl(Vec<u8>),
        #[prost(bytes, tag = "7")]
        InteractionQuery(Vec<u8>),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    pub model_details: Option<ModelDetails>,
    #[prost(message, optional, tag = "9")]
    pub requested_model: Option<RequestedModel>,
    #[prost(string, optional, tag = "5")]
    pub conversation_id: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "8")]
    pub turns: Vec<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ModelDetails {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(message, optional, tag = "2")]
    pub thinking_details: Option<Empty>,
    #[prost(string, tag = "3")]
    pub display_model_id: String,
    #[prost(string, tag = "4")]
    pub display_name: String,
    #[prost(string, tag = "5")]
    pub display_name_short: String,
    #[prost(string, repeated, tag = "6")]
    pub aliases: Vec<String>,
    #[prost(bool, optional, tag = "7")]
    pub max_mode: Option<bool>,
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<ModelParameter>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsRequest {
    #[prost(string, repeated, tag = "1")]
    pub custom_model_ids: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub models: Vec<ModelDetails>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1,2")]
    pub action: Option<conversation_action::Action>,
}
pub mod conversation_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Action {
        #[prost(message, tag = "1")]
        User(super::UserMessageAction),
        #[prost(message, tag = "2")]
        Resume(super::ResumeAction),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ResumeAction {}
#[derive(Clone, PartialEq, Message)]
pub struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub message_id: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct TextMessage {
    #[prost(string, tag = "1")]
    pub text: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConversationTurnStructure {
    #[prost(message, optional, tag = "1")]
    pub agent_conversation_turn: Option<AgentConversationTurnStructure>,
}
#[derive(Clone, PartialEq, Message)]
pub struct AgentConversationTurnStructure {
    #[prost(bytes = "vec", tag = "1")]
    pub user_message: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub steps: Vec<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConversationStep {
    #[prost(oneof = "conversation_step::Message", tags = "1,2,3")]
    pub message: Option<conversation_step::Message>,
}
pub mod conversation_step {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Assistant(super::TextMessage),
        #[prost(message, tag = "2")]
        Tool(super::ToolCall),
        #[prost(message, tag = "3")]
        Thinking(super::TextMessage),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ToolCall {
    #[prost(message, optional, tag = "15")]
    pub mcp_tool_call: Option<McpToolCall>,
    #[prost(string, optional, tag = "57")]
    pub tool_call_id: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpToolCall {
    #[prost(message, optional, tag = "1")]
    pub args: Option<McpArgs>,
    #[prost(message, optional, tag = "2")]
    pub result: Option<McpToolResult>,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpArgs {
    #[prost(string, tag = "1")]
    pub name: String,
    // Values are serialized google.protobuf.Value, NOT JSON bytes or Struct.
    #[prost(btree_map = "string, bytes", tag = "2")]
    pub args: BTreeMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
    #[prost(bool, tag = "7")]
    pub smart_mode_approval_only: bool,
    #[prost(string, tag = "9")]
    pub server_identifier: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpToolResult {
    #[prost(oneof = "mcp_tool_result::Result", tags = "1,2")]
    pub result: Option<mcp_tool_result::Result>,
}
pub mod mcp_tool_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::McpSuccess),
        #[prost(message, tag = "2")]
        Error(super::ErrorText),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ErrorText {
    #[prost(string, tag = "1")]
    pub error: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpSuccess {
    #[prost(message, repeated, tag = "1")]
    pub content: Vec<McpContent>,
    #[prost(bool, tag = "2")]
    pub is_error: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpContent {
    #[prost(message, optional, tag = "1")]
    pub text: Option<TextMessage>,
}
#[derive(Clone, PartialEq, Message)]
pub struct McpToolDefinition {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(bytes = "vec", tag = "3")]
    pub input_schema: Vec<u8>,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct CursorRule {
    #[prost(string, tag = "1")]
    pub full_path: String,
    #[prost(string, tag = "2")]
    pub content: String,
    #[prost(message, optional, tag = "3")]
    pub r#type: Option<CursorRuleType>,
    #[prost(int32, tag = "4")]
    pub source: i32,
}
#[derive(Clone, PartialEq, Message)]
pub struct CursorRuleType {
    #[prost(message, optional, tag = "1")]
    pub global: Option<Empty>,
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContext {
    #[prost(message, repeated, tag = "2")]
    pub rules: Vec<CursorRule>,
    #[prost(message, repeated, tag = "7")]
    pub tools: Vec<McpToolDefinition>,
    #[prost(bool, optional, tag = "17")]
    pub web_search_enabled: Option<bool>,
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextResult {
    #[prost(message, optional, tag = "1")]
    pub success: Option<RequestContextSuccess>,
}
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    pub request_context: Option<RequestContext>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(
        oneof = "exec_server_message::Message",
        tags = "2,3,4,5,7,8,9,10,11,14,16,17,18,20,21,22,23,27,28,29,30,31,36,37,38,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54"
    )]
    pub message: Option<exec_server_message::Message>,
}
pub mod exec_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(bytes, tag = "2")]
        Shell(Vec<u8>),
        #[prost(bytes, tag = "3")]
        Write(Vec<u8>),
        #[prost(bytes, tag = "4")]
        Delete(Vec<u8>),
        #[prost(bytes, tag = "5")]
        Grep(Vec<u8>),
        #[prost(bytes, tag = "7")]
        Read(Vec<u8>),
        #[prost(bytes, tag = "8")]
        Ls(Vec<u8>),
        #[prost(bytes, tag = "9")]
        Diagnostics(Vec<u8>),
        #[prost(message, tag = "10")]
        RequestContext(super::Empty),
        #[prost(message, tag = "11")]
        Mcp(super::McpArgs),
        #[prost(bytes, tag = "14")]
        ShellStream(Vec<u8>),
        #[prost(bytes, tag = "16")]
        BackgroundShell(Vec<u8>),
        #[prost(bytes, tag = "17")]
        ListMcpResources(Vec<u8>),
        #[prost(bytes, tag = "18")]
        ReadMcpResource(Vec<u8>),
        #[prost(bytes, tag = "20")]
        Fetch(Vec<u8>),
        #[prost(bytes, tag = "21")]
        RecordScreen(Vec<u8>),
        #[prost(bytes, tag = "22")]
        ComputerUse(Vec<u8>),
        #[prost(bytes, tag = "23")]
        WriteShellStdin(Vec<u8>),
        #[prost(bytes, tag = "27")]
        ExecuteHook(Vec<u8>),
        #[prost(bytes, tag = "28")]
        Subagent(Vec<u8>),
        #[prost(bytes, tag = "29")]
        RedactedRead(Vec<u8>),
        #[prost(bytes, tag = "30")]
        ForceBackgroundShell(Vec<u8>),
        #[prost(bytes, tag = "31")]
        ForceBackgroundSubagent(Vec<u8>),
        #[prost(bytes, tag = "36")]
        McpState(Vec<u8>),
        #[prost(bytes, tag = "37")]
        SubagentAwait(Vec<u8>),
        #[prost(bytes, tag = "38")]
        SmartModeClassifier(Vec<u8>),
        #[prost(bytes, tag = "40")]
        CanvasDiagnostics(Vec<u8>),
        #[prost(bytes, tag = "41")]
        ShellAllowlist(Vec<u8>),
        #[prost(bytes, tag = "42")]
        McpAllowlist(Vec<u8>),
        #[prost(bytes, tag = "43")]
        WebFetchAllowlist(Vec<u8>),
        #[prost(bytes, tag = "44")]
        GitDiff(Vec<u8>),
        #[prost(bytes, tag = "45")]
        PiRead(Vec<u8>),
        #[prost(bytes, tag = "46")]
        PiBash(Vec<u8>),
        #[prost(bytes, tag = "47")]
        PiEdit(Vec<u8>),
        #[prost(bytes, tag = "48")]
        PiWrite(Vec<u8>),
        #[prost(bytes, tag = "49")]
        PiGrep(Vec<u8>),
        #[prost(bytes, tag = "50")]
        PiFind(Vec<u8>),
        #[prost(bytes, tag = "51")]
        PiLs(Vec<u8>),
        #[prost(bytes, tag = "52")]
        MiniSweBash(Vec<u8>),
        #[prost(bytes, tag = "53")]
        ConversationSearch(Vec<u8>),
        #[prost(bytes, tag = "54")]
        AgentStoreConflict(Vec<u8>),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(message, optional, tag = "10")]
    pub request_context_result: Option<RequestContextResult>,
    #[prost(message, optional, tag = "2")]
    pub shell_result: Option<ShellRejectResult>,
    #[prost(message, optional, tag = "3")]
    pub write_result: Option<WriteDeleteRejectResult>,
    #[prost(message, optional, tag = "4")]
    pub delete_result: Option<WriteDeleteRejectResult>,
    #[prost(message, optional, tag = "5")]
    pub grep_result: Option<GrepErrorResult>,
    #[prost(message, optional, tag = "7")]
    pub read_result: Option<ReadLsRejectResult>,
    #[prost(message, optional, tag = "8")]
    pub ls_result: Option<ReadLsRejectResult>,
}
#[derive(Clone, PartialEq, Message)]
pub struct PathArgs {
    #[prost(string, tag = "1")]
    pub path: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct PathRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellRejected {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(string, tag = "3")]
    pub reason: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct ShellRejectResult {
    #[prost(message, optional, tag = "4")]
    pub rejected: Option<ShellRejected>,
}
#[derive(Clone, PartialEq, Message)]
pub struct WriteDeleteRejectResult {
    #[prost(message, optional, tag = "6")]
    pub rejected: Option<PathRejected>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ReadLsRejectResult {
    #[prost(message, optional, tag = "3")]
    pub rejected: Option<PathRejected>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GrepErrorResult {
    #[prost(message, optional, tag = "2")]
    pub error: Option<ErrorText>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "1,2")]
    pub message: Option<exec_client_control_message::Message>,
}
pub mod exec_client_control_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Close(super::ExecClientStreamClose),
        #[prost(message, tag = "2")]
        Throw(super::ExecClientThrow),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientThrow {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub error: String,
    #[prost(string, optional, tag = "4")]
    pub error_code: Option<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2,3")]
    pub message: Option<kv_server_message::Message>,
}
pub mod kv_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        Get(super::GetBlobArgs),
        #[prost(message, tag = "3")]
        Set(super::SetBlobArgs),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2,3")]
    pub message: Option<kv_client_message::Message>,
}
pub mod kv_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        Get(super::GetBlobResult),
        #[prost(message, tag = "3")]
        Set(super::Empty),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct GetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct GetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub blob_data: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, Message)]
pub struct SetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub blob_data: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(
        oneof = "interaction_update::Message",
        tags = "1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17"
    )]
    pub message: Option<interaction_update::Message>,
}
pub mod interaction_update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Text(super::TextMessage),
        #[prost(message, tag = "2")]
        ToolStarted(super::ToolCallUpdate),
        #[prost(message, tag = "3")]
        ToolCompleted(super::ToolCallUpdate),
        #[prost(message, tag = "4")]
        Thinking(super::TextMessage),
        #[prost(message, tag = "5")]
        ThinkingCompleted(super::Empty),
        #[prost(message, tag = "6")]
        UserAppended(super::Empty),
        #[prost(message, tag = "7")]
        PartialTool(super::PartialToolCallUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(super::Empty),
        #[prost(message, tag = "9")]
        Summary(super::Empty),
        #[prost(message, tag = "10")]
        SummaryStarted(super::Empty),
        #[prost(message, tag = "11")]
        SummaryCompleted(super::Empty),
        #[prost(bytes, tag = "12")]
        ShellOutput(Vec<u8>),
        #[prost(message, tag = "13")]
        Heartbeat(super::Empty),
        #[prost(message, tag = "14")]
        TurnEnded(super::TurnEndedUpdate),
        #[prost(bytes, tag = "15")]
        ToolDelta(Vec<u8>),
        #[prost(message, tag = "16")]
        StepStarted(super::Empty),
        #[prost(message, tag = "17")]
        StepCompleted(super::Empty),
    }
}
#[derive(Clone, PartialEq, Message)]
pub struct ToolCallUpdate {
    #[prost(string, tag = "1")]
    pub call_id: String,
    #[prost(message, optional, tag = "2")]
    pub tool_call: Option<ToolCall>,
}
#[derive(Clone, PartialEq, Message)]
pub struct PartialToolCallUpdate {
    #[prost(string, tag = "1")]
    pub call_id: String,
    #[prost(message, optional, tag = "2")]
    pub tool_call: Option<ToolCall>,
    #[prost(string, tag = "3")]
    pub args_text_delta: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct TurnEndedUpdate {
    #[prost(int64, optional, tag = "1")]
    pub input_tokens: Option<i64>,
    #[prost(int64, optional, tag = "2")]
    pub output_tokens: Option<i64>,
    #[prost(int64, optional, tag = "3")]
    pub cache_read_tokens: Option<i64>,
    #[prost(int64, optional, tag = "4")]
    pub cache_write_tokens: Option<i64>,
    #[prost(int64, optional, tag = "5")]
    pub reasoning_tokens: Option<i64>,
}
