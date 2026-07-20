use std::{collections::HashMap, sync::Arc};

use crate::{ai::agent::redaction, terminal::model::session::SessionType};
use futures_util::StreamExt;
use warp_core::features::FeatureFlag;
use warp_multi_agent_api as api;

use crate::server::server_api::ServerApi;

use super::{convert_to::convert_input, ConvertToAPITypeError, RequestParams, ResponseStream};

pub async fn generate_multi_agent_output(
    server_api: Arc<ServerApi>,
    mut params: RequestParams,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> Result<ResponseStream, ConvertToAPITypeError> {
    // hw-o8h fail-closed lookup: if `from_session_for_view` was used to
    // build this `SessionContext` and could not locate the owning
    // PaneGroup for the request's terminal view, we MUST NOT route to
    // the local / hosted / OpenRouter / helm_oz fallback. We have no
    // authoritative answer about what tab the request is for, so a
    // silent fall-through would be indistinguishable from a cross-talk
    // bug. Surface a typed error before any conversion or routing.
    if params
        .session_context
        .has_unresolved_helm_binding_lookup()
    {
        log::error!(
            "helm: refusing request — SessionContext has unresolved helm tab binding lookup \
             (terminal view not found). Will not route to local / OpenRouter / hosted endpoint."
        );
        return Err(ConvertToAPITypeError::HelmTabLookupFailed);
    }

    let supported_tools = params
        .supported_tools_override
        .take()
        .unwrap_or_else(|| get_supported_tools(&params));
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);
    let mut logging_metadata = HashMap::new();
    if let Some(metadata) = params.metadata {
        logging_metadata.insert(
            "is_autodetected_user_query".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_autodetected_user_query,
                )),
            },
        );
        logging_metadata.insert(
            "entrypoint".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    metadata.entrypoint.entrypoint(),
                )),
            },
        );
        logging_metadata.insert(
            "is_auto_resume_after_error".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_auto_resume_after_error,
                )),
            },
        );
    }

    if params.should_redact_secrets {
        redaction::redact_inputs(&mut params.input);
    }

    let api_keys = api_keys_with_warp_credit_fallback_setting(
        params.api_keys,
        params.allow_use_of_warp_credits,
    );

    // Remote-bound requests must not bleed local context (shell/os) into the
    // wire payload; the conversation is endpoint-bound, not host-bound.
    // `is_remote_or_helm_bound` covers both cases: the Mac terminal
    // session is `WarpifiedRemote`, OR the helm tab binding says this
    // tab is bound to a remote endpoint (the Mac shell underneath may
    // still be local — hw-o8h).
    let is_remote_or_helm_bound = params.session_context.is_remote()
        || params.session_context.is_helm_remote();

    let request = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: params.tasks,
        }),
        input: Some(convert_input(params.input, is_remote_or_helm_bound)?),
        settings: Some(api::request::Settings {
            model_config: Some(api::request::settings::ModelConfig {
                base: params.model.into(),
                cli_agent: params.cli_agent_model.into(),
                computer_use_agent: params.computer_use_model.into(),
                base_model_context_window_limit: if FeatureFlag::ConfigurableContextWindow
                    .is_enabled()
                {
                    params.context_window_limit.unwrap_or(0)
                } else {
                    0
                },
                ..Default::default()
            }),
            rules_enabled: params.is_memory_enabled,
            warp_drive_context_enabled: params.warp_drive_context_enabled,
            web_context_retrieval_enabled: true,
            supports_parallel_tool_calls: true,
            use_anthropic_text_editor_tools: false,
            planning_enabled: params.planning_enabled,
            supports_create_files: true,
            supported_tools: supported_tools.into_iter().map(Into::into).collect(),
            supports_long_running_commands: true,
            should_preserve_file_content_in_history: true,
            supports_todos_ui: true,
            supports_linked_code_blocks: FeatureFlag::LinkedCodeBlocks.is_enabled(),
            supports_started_child_task_message: true,
            supports_suggest_prompt: true,
            supports_read_image_files: FeatureFlag::ReadImageFiles.is_enabled(),
            supports_reasoning_message: true,
            api_keys,
            autonomy_level: params.autonomy_level.into(),
            isolation_level: params.isolation_level.into(),
            web_search_enabled: params.web_search_enabled,
            supported_cli_agent_tools: supported_cli_agent_tools
                .into_iter()
                .map(Into::into)
                .collect(),
            supports_v4a_file_diffs: FeatureFlag::V4AFileDiffs.is_enabled(),
            supports_summarization_via_message_replacement:
                FeatureFlag::SummarizationViaMessageReplacement.is_enabled(),
            supports_bundled_skills: FeatureFlag::BundledSkills.is_enabled(),
            supports_research_agent: params.research_agent_enabled,
            supports_orchestration_v2: FeatureFlag::OrchestrationV2.is_enabled(),
            custom_model_providers: params.custom_model_providers,
        }),
        metadata: Some(api::request::Metadata {
            logging: logging_metadata,
            conversation_id: params
                .conversation_token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            ambient_agent_task_id: params
                .ambient_agent_task_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            forked_from_conversation_id: if params.conversation_token.is_none() {
                // We only include this param on our initial request to the server
                // (when the forked conversation has not been assigned a new id yet).
                params
                    .forked_from_conversation_token
                    .map(|token| token.as_str().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            },
            parent_agent_id: params.parent_agent_id.unwrap_or_default(),
            agent_name: params.agent_name.unwrap_or_default(),
        }),
        existing_suggestions: params
            .existing_suggestions
            .map(|suggestions| suggestions.into()),
        mcp_context: params.mcp_context.map(Into::into),
    };

    let response_stream = server_api
        .generate_multi_agent_output(
            &request,
            params.session_context.helm_tab_binding(),
        )
        .await;
    match response_stream {
        Ok(stream) => {
            let output_stream = stream.take_until(cancellation_rx);
            Ok(Box::pin(output_stream))
        }
        Err(e) => {
            let (tx, rx) = async_channel::unbounded();
            let _ = tx.send(Err(e)).await;
            Ok(Box::pin(rx))
        }
    }
}

fn api_keys_with_warp_credit_fallback_setting(
    api_keys: Option<api::request::settings::ApiKeys>,
    allow_use_of_warp_credits: bool,
) -> Option<api::request::settings::ApiKeys> {
    match api_keys {
        Some(mut api_keys) => {
            api_keys.allow_use_of_warp_credits = allow_use_of_warp_credits;
            Some(api_keys)
        }
        None if allow_use_of_warp_credits => Some(api::request::settings::ApiKeys {
            allow_use_of_warp_credits: true,
            ..Default::default()
        }),
        None => None,
    }
}
fn get_supported_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_tools = vec![
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
        api::ToolType::ReadMcpResource,
        api::ToolType::CallMcpTool,
        api::ToolType::InitProject,
        api::ToolType::OpenCodeReview,
        api::ToolType::RunShellCommand,
        api::ToolType::SuggestNewConversation,
        api::ToolType::Subagent,
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::ReadDocuments,
        api::ToolType::CreateDocuments,
        api::ToolType::EditDocuments,
        api::ToolType::SuggestPrompt,
    ];

    if FeatureFlag::ConversationsAsContext.is_enabled() {
        supported_tools.push(api::ToolType::FetchConversation);
    }

    match params.session_context.effective_session_type().0 {
        None | Some(SessionType::Local) => {
            supported_tools.extend(&[
                api::ToolType::ReadFiles,
                api::ToolType::ApplyFileDiffs,
                api::ToolType::SearchCodebase,
            ]);

            if FeatureFlag::ArtifactCommand.is_enabled() {
                supported_tools.push(api::ToolType::UploadFileArtifact);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            // Remote-bound tabs must advertise only endpoint-routable tools.
            // RunShellCommand / WriteToLongRunningShellCommand / ReadShellCommandOutput
            // are local-shell fallbacks — never execute on the user's host. They
            // are stripped at request construction so the model never asks for
            // them; the executor also rejects any straggler tool call below.
            supported_tools.retain(|tool| {
                !matches!(
                    tool,
                    api::ToolType::RunShellCommand
                        | api::ToolType::WriteToLongRunningShellCommand
                        | api::ToolType::ReadShellCommandOutput
                )
            });

            supported_tools.extend(&[api::ToolType::ReadFiles, api::ToolType::ApplyFileDiffs]);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {
            // Feature flag off or not yet connected — no remote tools. Even so,
            // strip local fallback shell tools so a downgrade never executes
            // them on the user's host.
            supported_tools.retain(|tool| {
                !matches!(
                    tool,
                    api::ToolType::RunShellCommand
                        | api::ToolType::WriteToLongRunningShellCommand
                        | api::ToolType::ReadShellCommandOutput
                )
            });
        }
    }

    if FeatureFlag::AgentModeComputerUse.is_enabled() && params.computer_use_enabled {
        supported_tools.extend(&[api::ToolType::UseComputer]);
        supported_tools.extend(&[api::ToolType::RequestComputerUse])
    }

    if FeatureFlag::PRCommentsSlashCommand.is_enabled() {
        supported_tools.push(api::ToolType::InsertReviewComments);
    }

    if FeatureFlag::ListSkills.is_enabled() {
        supported_tools.push(api::ToolType::ReadSkill);
    }

    if params.orchestration_enabled {
        // Always advertise the legacy start-agent tool so the server
        // can fall back to it when its own orchestrate flag is off.
        // When RunAgents is also enabled, advertise it alongside.
        supported_tools.push(if FeatureFlag::OrchestrationV2.is_enabled() {
            api::ToolType::StartAgentV2
        } else {
            api::ToolType::StartAgent
        });
        if FeatureFlag::RunAgentsTool.is_enabled() && FeatureFlag::OrchestrationV2.is_enabled() {
            supported_tools.push(api::ToolType::RunAgents);
        }
        supported_tools.push(api::ToolType::SendMessageToAgent);
    }

    if FeatureFlag::AskUserQuestion.is_enabled() && params.ask_user_question_enabled {
        supported_tools.push(api::ToolType::AskUserQuestion);
    }

    supported_tools
}

fn get_supported_cli_agent_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_cli_agent_tools = vec![
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
    ];

    match params.session_context.effective_session_type().0 {
        None | Some(SessionType::Local) => {
            supported_cli_agent_tools
                .extend(&[api::ToolType::ReadFiles, api::ToolType::SearchCodebase]);
            // TransferShellCommandControlToUser is a local-shell fallback
            // — only the local path may advertise it (paired with the
            // WarpifiedRemote strip below).
            if FeatureFlag::TransferControlTool.is_enabled() {
                supported_cli_agent_tools.push(api::ToolType::TransferShellCommandControlToUser);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            // Same guard as `get_supported_tools`: local-fallback shell tools
            // are not routable to the endpoint.
            // hw-o8h: also strip TransferShellCommandControlToUser — it
            // hands control of a *local* shell process back to the user,
            // which has no meaning on an endpoint-bound tab.
            supported_cli_agent_tools.retain(|tool| {
                !matches!(
                    tool,
                    api::ToolType::WriteToLongRunningShellCommand
                        | api::ToolType::ReadShellCommandOutput
                        | api::ToolType::TransferShellCommandControlToUser
                )
            });
            supported_cli_agent_tools.push(api::ToolType::ReadFiles);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_cli_agent_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {
            // Downgrade: still strip local-fallback shell tools
            // (and TransferShellCommandControlToUser for the same reason).
            supported_cli_agent_tools.retain(|tool| {
                !matches!(
                    tool,
                    api::ToolType::WriteToLongRunningShellCommand
                        | api::ToolType::ReadShellCommandOutput
                        | api::ToolType::TransferShellCommandControlToUser
                )
            });
        }
    }

    supported_cli_agent_tools
}

#[cfg(test)]
#[path = "impl_tests.rs"]
mod tests;
