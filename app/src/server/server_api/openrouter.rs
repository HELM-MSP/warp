//! Helm OpenRouter BYOK adapter.
//!
//! When `HELM_OPENROUTER_API_KEY` is present, multi-agent requests are routed to
//! OpenRouter's `/api/v1/chat/completions` endpoint instead of Warp's hosted
//! `/ai/multi-agent` endpoint. The adapter translates the Warp protobuf request
//! into an OpenAI-compatible chat completion request and maps the streamed
//! completion chunks back into `warp_multi_agent_api::ResponseEvent` client
//! actions so the rest of the client continues to work unchanged.

use std::sync::Arc;

use anyhow::Context as _;
use async_stream::stream;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{AIApiError, AIOutputStream};

/// Environment variable holding the OpenRouter API key.
pub const HELM_OPENROUTER_API_KEY_ENV: &str = "HELM_OPENROUTER_API_KEY";

/// Optional override for the OpenRouter model slug.
pub const HELM_OPENROUTER_MODEL_ENV: &str = "HELM_OPENROUTER_MODEL_ENV";

/// Optional override for the client-facing adapter identifier.
pub const HELM_CLIENT_OPENROUTER_ADAPTER_ENV: &str = "HELM_CLIENT_OPENROUTER_ADAPTER_ENV";

/// Default OpenRouter model used when no override is provided.
pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-4.1-mini";

/// OpenRouter chat completions endpoint.
pub const OPENROUTER_CHAT_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Returns `true` when the Helm OpenRouter BYOK path is enabled.
pub fn is_openrouter_adapter_enabled() -> bool {
    std::env::var(HELM_OPENROUTER_API_KEY_ENV).is_ok_and(|key| !key.trim().is_empty())
}

/// Context describing where a command/tool is executed in the Helm adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HelmLocalExecutionContext {
    /// Execution happens in the user's local shell environment.
    Local,
    /// Execution is proxied through an external endpoint.
    Endpoint,
}

/// Metadata about an endpoint used by the Helm adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct HelmEndpointExecutionContext {
    /// Human-readable endpoint name.
    pub name: String,
    /// Base URL for the endpoint.
    pub base_url: String,
}

impl HelmEndpointExecutionContext {
    #[allow(dead_code)]
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
        }
    }
}

/// Risk category for a command proposed by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "snake_case")]
pub enum HelmCommandRiskCategory {
    /// Read-only or otherwise non-destructive.
    ReadOnly,
    /// Potentially destructive; requires user approval.
    Destructive,
    /// Unknown risk; treat conservatively.
    Unknown,
}

impl Default for HelmCommandRiskCategory {
    fn default() -> Self {
        Self::Unknown
    }
}

// ---------------------------------------------------------------------------
// OpenRouter request/response JSON types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterChatCompletionRequest {
    model: String,
    messages: Vec<OpenRouterMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenRouterTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenRouterToolChoice>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterMessage {
    role: OpenRouterRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenRouterToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OpenRouterRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterTool {
    r#type: String,
    function: OpenRouterToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum OpenRouterToolChoice {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "function")]
    Function { function: OpenRouterNamedFunction },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterNamedFunction {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterToolCall {
    id: String,
    r#type: String,
    function: OpenRouterToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterChatCompletionResponse {
    id: String,
    model: String,
    choices: Vec<OpenRouterChoice>,
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterChoice {
    index: u32,
    message: Option<OpenRouterMessage>,
    delta: Option<OpenRouterDelta>,
    #[serde(rename = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterDelta {
    role: Option<OpenRouterRole>,
    content: Option<String>,
    tool_calls: Option<Vec<OpenRouterToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OpenRouterUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterErrorResponse {
    error: OpenRouterErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterErrorDetail {
    message: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<u32>,
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

/// Generates a multi-agent response by routing the request to OpenRouter.
pub async fn generate_helm_openrouter_output(
    client: &http_client::Client,
    request: &warp_multi_agent_api::Request,
) -> std::result::Result<AIOutputStream<warp_multi_agent_api::ResponseEvent>, Arc<AIApiError>> {
    let api_key = std::env::var(HELM_OPENROUTER_API_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
        .context("HELM_OPENROUTER_API_KEY is not set")
        .map_err(|e| Arc::new(AIApiError::Other(e)))?;

    let model = std::env::var(HELM_OPENROUTER_MODEL_ENV)
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OPENROUTER_MODEL.to_string());

    let _adapter_tag = std::env::var(HELM_CLIENT_OPENROUTER_ADAPTER_ENV)
        .ok()
        .filter(|tag| !tag.trim().is_empty());

    log::info!(
        "helm: OpenRouter adapter handling multi-agent request (model={model})"
    );

    let openrouter_request = build_openrouter_request(request, model);
    let conversation_id = conversation_id_from_request(request);
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    let assistant_message_id = uuid::Uuid::new_v4().to_string();
    let task_id = primary_task_id(request);

    let body = serde_json::to_vec(&openrouter_request)
        .context("failed to serialize OpenRouter request")
        .map_err(|e| Arc::new(AIApiError::Other(e)))?;

    let response = client
        .post(OPENROUTER_CHAT_COMPLETIONS_URL)
        .header(http::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| Arc::new(AIApiError::Transport(e)))?;

    log::info!("helm: OpenRouter responded with status {}", response.status());

    if let Err(err) = response.error_for_status_ref() {
        let status = err.source.status().unwrap_or(http::StatusCode::BAD_REQUEST);
        let body_text = response.text().await.unwrap_or_default();
        let message = parse_openrouter_error(body_text, status);
        return Err(Arc::new(AIApiError::ErrorStatus(status, message)));
    }

    let stream = response
        .bytes_stream()
        .map_err(|e| Arc::new(AIApiError::Transport(e)));

    let output_stream = stream! {
        let init = warp_multi_agent_api::ResponseEvent {
            r#type: Some(warp_multi_agent_api::response_event::Type::Init(
                warp_multi_agent_api::response_event::StreamInit {
                    conversation_id: conversation_id.clone(),
                    request_id: request_id.clone(),
                    run_id: run_id.clone(),
                },
            )),
        };
        yield Ok(init);

        // Helm BYOK: the adapter fabricates its own task_id (the client never saw a
        // server `CreateTask`), so we must emit one ourselves before any
        // `AddMessagesToTask` / `AppendToMessageContent`. Without this, every
        // downstream action is rejected with TaskNotFound / ExchangeNotFound and
        // nothing renders. Mirrors the canonical sequence in
        // replay_agent_conversations: CreateTask with empty messages, then
        // AddMessagesToTask adds the (empty) anchor message, then content is
        // streamed via AppendToMessageContent.
        let create_task = warp_multi_agent_api::ResponseEvent {
            r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
                warp_multi_agent_api::response_event::ClientActions {
                    actions: vec![warp_multi_agent_api::ClientAction {
                        action: Some(warp_multi_agent_api::client_action::Action::CreateTask(
                            warp_multi_agent_api::client_action::CreateTask {
                                task: Some(warp_multi_agent_api::Task {
                                    id: task_id.clone(),
                                    description: String::new(),
                                    dependencies: None,
                                    messages: Vec::new(),
                                    summary: String::new(),
                                    server_data: String::new(),
                                }),
                            },
                        )),
                    }],
                },
            )),
        };
        yield Ok(create_task);

        // Buffer the entire OpenRouter stream into one string, then emit a SINGLE
        // `AddMessagesToTask` carrying the full text. We deliberately do NOT use
        // `AppendToMessageContent` here: in account-free BYOK mode the append
        // streaming path does not render or persist, even though it returns Ok.
        // Putting the complete text in `AddMessagesToTask` is the protocol used
        // by `helm_multi_agent_mock` and is the path proven to render.
        let mut current_content = String::new();

        for await chunk_result in stream {
            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(e) => {
                    yield Err(e);
                    continue;
                }
            };

            let text = match String::from_utf8(chunk.to_vec()) {
                Ok(text) => text,
                Err(_) => continue,
            };

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line == ":" {
                    continue;
                }

                let data = if let Some(stripped) = line.strip_prefix("data: ") {
                    stripped.trim()
                } else {
                    continue;
                };

                if data == "[DONE]" {
                    break;
                }

                let completion: OpenRouterChatCompletionResponse = match serde_json::from_str(data) {
                    Ok(completion) => completion,
                    Err(_) => continue,
                };

                for choice in completion.choices {
                    let delta = match choice.delta {
                        Some(delta) => delta,
                        None => continue,
                    };

                    if let Some(content) = delta.content {
                        if !content.is_empty() {
                            current_content.push_str(&content);
                        }
                    }

                    if let Some(tool_calls) = delta.tool_calls {
                        for tool_call in tool_calls {
                            if let Some(message) = openrouter_tool_call_to_warp_message(
                                &tool_call,
                                &task_id,
                                &request_id,
                            ) {
                                yield Ok(build_add_tool_call_event(
                                    &task_id,
                                    message,
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Emit ONE AddMessagesToTask with the complete buffered text, matching
        // the protocol used by `helm_multi_agent_mock`.
        let create_message = warp_multi_agent_api::ResponseEvent {
            r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
                warp_multi_agent_api::response_event::ClientActions {
                    actions: vec![warp_multi_agent_api::ClientAction {
                        action: Some(warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.clone(),
                                messages: vec![warp_multi_agent_api::Message {
                                    id: assistant_message_id.clone(),
                                    task_id: task_id.clone(),
                                    request_id: request_id.clone(),
                                    timestamp: Some(prost_types::Timestamp::from(
                                        std::time::SystemTime::now(),
                                    )),
                                    server_message_data: String::new(),
                                    citations: Vec::new(),
                                    message: Some(warp_multi_agent_api::message::Message::AgentOutput(
                                        warp_multi_agent_api::message::AgentOutput {
                                            text: current_content.clone(),
                                        },
                                    )),
                                }],
                            },
                        )),
                    }],
                },
            )),
        };
        log::info!(
            "helm: OpenRouter adapter emitted response with {} bytes of content",
            current_content.len()
        );
        yield Ok(create_message);

        let finished = warp_multi_agent_api::ResponseEvent {
            r#type: Some(warp_multi_agent_api::response_event::Type::Finished(
                warp_multi_agent_api::response_event::StreamFinished {
                    reason: Some(warp_multi_agent_api::response_event::stream_finished::Reason::Done(
                        warp_multi_agent_api::response_event::stream_finished::Done {},
                    )),
                    token_usage: Vec::new(),
                    should_refresh_model_config: false,
                    request_cost: None,
                    conversation_usage_metadata: None,
                },
            )),
        };
        yield Ok(finished);
    }
    .boxed();

    Ok(output_stream)
}

fn parse_openrouter_error(body_text: String, status: http::StatusCode) -> String {
    if body_text.trim().is_empty() {
        return format!("OpenRouter returned {status}");
    }

    serde_json::from_str::<OpenRouterErrorResponse>(&body_text)
        .map(|error_response| error_response.error.message)
        .unwrap_or_else(|_| body_text)
}

fn build_openrouter_request(
    request: &warp_multi_agent_api::Request,
    model: String,
) -> OpenRouterChatCompletionRequest {
    let mut messages = Vec::new();

    // System prompt that informs the model it is acting as Warp's agent mode.
    messages.push(OpenRouterMessage {
        role: OpenRouterRole::System,
        content: Some(
            "You are Warp's agent mode assistant. Help the user with coding, terminal, and shell tasks."
                .to_string(),
        ),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });

    // Include prior conversation history from the primary task, if any.
    if let Some(task_context) = &request.task_context {
        for task in &task_context.tasks {
            for message in &task.messages {
                if let Some(openrouter_message) = warp_message_to_openrouter_message(message) {
                    messages.push(openrouter_message);
                }
            }
        }
    }

    // Append the current user input, if present.
    if let Some(user_message) = current_user_message(request) {
        messages.push(user_message);
    }

    let tools = supported_tools(request);
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some(OpenRouterToolChoice::Auto)
    };

    OpenRouterChatCompletionRequest {
        model,
        messages,
        tools,
        tool_choice,
        stream: true,
        temperature: None,
        max_tokens: None,
    }
}

fn warp_message_to_openrouter_message(
    message: &warp_multi_agent_api::Message,
) -> Option<OpenRouterMessage> {
    use warp_multi_agent_api::message::Message;

    match message.message.as_ref()? {
        Message::UserQuery(user_query) => Some(OpenRouterMessage {
            role: OpenRouterRole::User,
            content: Some(user_query.query.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }),
        Message::AgentOutput(agent_output) => Some(OpenRouterMessage {
            role: OpenRouterRole::Assistant,
            content: Some(agent_output.text.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }),
        Message::ToolCall(tool_call) => {
            let openrouter_tool_call = warp_tool_call_to_openrouter_tool_call(tool_call)?;
            Some(OpenRouterMessage {
                role: OpenRouterRole::Assistant,
                content: None,
                tool_calls: Some(vec![openrouter_tool_call]),
                tool_call_id: None,
                name: None,
            })
        }
        Message::ToolCallResult(tool_call_result) => Some(OpenRouterMessage {
            role: OpenRouterRole::Tool,
            content: Some(tool_call_result_to_text(tool_call_result)),
            tool_calls: None,
            tool_call_id: Some(tool_call_result.tool_call_id.clone()),
            name: None,
        }),
        _ => None,
    }
}

#[allow(deprecated)]
fn current_user_message(request: &warp_multi_agent_api::Request) -> Option<OpenRouterMessage> {
    let input = request.input.as_ref()?;
    match input.r#type.as_ref()? {
        warp_multi_agent_api::request::input::Type::UserQuery(user_query) => {
            Some(OpenRouterMessage {
                role: OpenRouterRole::User,
                content: Some(user_query.query.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            })
        }
        warp_multi_agent_api::request::input::Type::UserInputs(user_inputs) => {
            for input in &user_inputs.inputs {
                match input.input.as_ref()? {
                    warp_multi_agent_api::request::input::user_inputs::user_input::Input::UserQuery(
                        user_query,
                    ) => {
                        return Some(OpenRouterMessage {
                            role: OpenRouterRole::User,
                            content: Some(user_query.query.clone()),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    _ => continue,
                }
            }
            None
        }
        _ => None,
    }
}

fn conversation_id_from_request(request: &warp_multi_agent_api::Request) -> String {
    request
        .metadata
        .as_ref()
        .and_then(|metadata| {
            if metadata.conversation_id.is_empty() {
                None
            } else {
                Some(metadata.conversation_id.clone())
            }
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn primary_task_id(request: &warp_multi_agent_api::Request) -> String {
    request
        .task_context
        .as_ref()
        .and_then(|task_context| task_context.tasks.first().map(|task| task.id.clone()))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn build_add_tool_call_event(
    task_id: &str,
    message: warp_multi_agent_api::Message,
) -> warp_multi_agent_api::ResponseEvent {
    warp_multi_agent_api::ResponseEvent {
        r#type: Some(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: task_id.to_string(),
                                messages: vec![message],
                            },
                        ),
                    ),
                }],
            },
        )),
    }
}

fn openrouter_tool_call_to_warp_message(
    tool_call: &OpenRouterToolCall,
    task_id: &str,
    request_id: &str,
) -> Option<warp_multi_agent_api::Message> {
    let warp_tool = openrouter_function_to_warp_tool(&tool_call.function)?;

    Some(warp_multi_agent_api::Message {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
        server_message_data: String::new(),
        citations: Vec::new(),
        message: Some(warp_multi_agent_api::message::Message::ToolCall(
            warp_multi_agent_api::message::ToolCall {
                tool_call_id: tool_call.id.clone(),
                tool: Some(warp_tool),
            },
        )),
    })
}

fn openrouter_function_to_warp_tool(
    function: &OpenRouterToolCallFunction,
) -> Option<warp_multi_agent_api::message::tool_call::Tool> {
    use warp_multi_agent_api::message::tool_call::Tool;

    match function.name.as_str() {
        "run_shell_command" => {
            let args: serde_json::Value = serde_json::from_str(&function.arguments).ok()?;
            let command = args.get("command")?.as_str()?.to_string();
            Some(Tool::RunShellCommand(
                warp_multi_agent_api::message::tool_call::RunShellCommand {
                    command,
                    is_read_only: false,
                    uses_pager: false,
                    citations: Vec::new(),
                    is_risky: false,
                    risk_category: warp_multi_agent_api::RiskCategory::Unspecified as i32,
                    wait_until_complete_value: None,
                },
            ))
        }
        _ => Some(Tool::Server(
            warp_multi_agent_api::message::tool_call::Server {
                payload: function.arguments.clone(),
            },
        )),
    }
}

fn warp_tool_call_to_openrouter_tool_call(
    tool_call: &warp_multi_agent_api::message::ToolCall,
) -> Option<OpenRouterToolCall> {
    use warp_multi_agent_api::message::tool_call::Tool;

    let (name, arguments) = match tool_call.tool.as_ref()? {
        Tool::RunShellCommand(run_shell_command) => (
            "run_shell_command".to_string(),
            json!({ "command": run_shell_command.command }),
        ),
        _ => return None,
    };

    Some(OpenRouterToolCall {
        id: tool_call.tool_call_id.clone(),
        r#type: "function".to_string(),
        function: OpenRouterToolCallFunction {
            name,
            arguments: arguments.to_string(),
        },
    })
}

fn tool_call_result_to_text(
    tool_call_result: &warp_multi_agent_api::message::ToolCallResult,
) -> String {
    // Best-effort text representation of a tool result for the LLM context.
    format!("{tool_call_result:?}")
}

fn supported_tools(request: &warp_multi_agent_api::Request) -> Vec<OpenRouterTool> {
    let settings = match request.settings.as_ref() {
        Some(settings) => settings,
        None => return Vec::new(),
    };

    let mut tools = Vec::new();

    if settings
        .supported_tools
        .contains(&(warp_multi_agent_api::ToolType::RunShellCommand as i32))
    {
        tools.push(OpenRouterTool {
            r#type: "function".to_string(),
            function: OpenRouterToolFunction {
                name: "run_shell_command".to_string(),
                description: Some("Run a shell command in the user's terminal.".to_string()),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute."
                        }
                    },
                    "required": ["command"]
                }),
            },
        });
    }

    tools
}
