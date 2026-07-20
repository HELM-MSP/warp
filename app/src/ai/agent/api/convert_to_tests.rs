use crate::ai::agent::task::TaskId;
use crate::ai::agent::{
    AIAgentActionResult, AIAgentActionResultType, AIAgentContext, BlockContext,
    TransferShellCommandControlToUserResult,
};
use crate::ai_assistant::execution_context::{WarpAiExecutionContext, WarpAiOsContext};
use crate::terminal::model::block::BlockId;
use chrono::DateTime;
use chrono::Utc;
use warp_core::command::ExitCode;
use warp_multi_agent_api as api;

use super::convert_context;

#[test]
fn transfer_control_snapshot_result_converts_to_tool_call_result_input() {
    let block_id = BlockId::default();
    let input =
        api::request::input::user_inputs::user_input::Input::try_from(AIAgentActionResult {
            id: "tool_call".to_string().into(),
            task_id: TaskId::new("task".to_string()),
            result: AIAgentActionResultType::TransferShellCommandControlToUser(
                TransferShellCommandControlToUserResult::Snapshot {
                    block_id: block_id.clone(),
                    grid_contents: "snapshot".to_string(),
                    cursor: "<|cursor|>".to_string(),
                    is_alt_screen_active: false,
                    is_preempted: false,
                },
            ),
        })
        .unwrap();

    match input {
        api::request::input::user_inputs::user_input::Input::ToolCallResult(result) => {
            assert_eq!(result.tool_call_id, "tool_call");
            match result.result {
                Some(api::request::input::tool_call_result::Result::TransferShellCommandControlToUser(
                    api_result,
                )) => match api_result.result {
                    Some(
                        api::transfer_shell_command_control_to_user_result::Result::LongRunningCommandSnapshot(snapshot),
                    ) => {
                        assert_eq!(snapshot.command_id, block_id.to_string());
                        assert_eq!(snapshot.output, "snapshot");
                        assert_eq!(snapshot.cursor, "<|cursor|>");
                    }
                    other => panic!("Expected snapshot result, got {other:?}"),
                },
                other => panic!("Expected transfer-control tool call result, got {other:?}"),
            }
        }
        other => panic!("Expected tool-call-result input, got {other:?}"),
    }
}

#[test]
fn transfer_control_finished_result_converts_to_tool_call_result_input() {
    let block_id = BlockId::default();
    let start_ts = DateTime::from(Utc::now());
    let completed_ts = DateTime::from(Utc::now());
    let input =
        api::request::input::user_inputs::user_input::Input::try_from(AIAgentActionResult {
            id: "tool_call".to_string().into(),
            task_id: TaskId::new("task".to_string()),
            result: AIAgentActionResultType::TransferShellCommandControlToUser(
                TransferShellCommandControlToUserResult::CommandFinished {
                    block_id: block_id.clone(),
                    output: "done".to_string(),
                    exit_code: ExitCode::from(17),
                    start_ts: Some(start_ts),
                    completed_ts: Some(completed_ts),
                },
            ),
        })
        .unwrap();

    match input {
        api::request::input::user_inputs::user_input::Input::ToolCallResult(result) => {
            assert_eq!(result.tool_call_id, "tool_call");
            match result.result {
                Some(api::request::input::tool_call_result::Result::TransferShellCommandControlToUser(
                    api_result,
                )) => match api_result.result {
                    Some(
                        api::transfer_shell_command_control_to_user_result::Result::CommandFinished(finished),
                    ) => {
                        assert_eq!(finished.command_id, block_id.to_string());
                        assert_eq!(finished.output, "done");
                        assert_eq!(finished.exit_code, 17);
                        assert_eq!(finished.start_ts, Some(super::local_datetime_to_timestamp(start_ts)));
                        assert_eq!(finished.finish_ts, Some(super::local_datetime_to_timestamp(completed_ts)));
                    }
                    other => panic!("Expected command-finished result, got {other:?}"),
                },
                other => panic!("Expected transfer-control tool call result, got {other:?}"),
            }
        }
        other => panic!("Expected tool-call-result input, got {other:?}"),
    }
}

// hw-c6z: remote-bound request bodies must never carry local shell/os info.
// macOS/zsh/local hostname must not leak into /ai/multi-agent/remote payloads.
#[test]
fn convert_context_drops_execution_environment_when_remote() {
    let ctx = vec![AIAgentContext::ExecutionEnvironment(WarpAiExecutionContext {
        os: WarpAiOsContext {
            category: Some("darwin".to_string()),
            distribution: Some("macOS".to_string()),
        },
        shell_name: "zsh".to_string(),
        shell_version: Some("5.9".to_string()),
    })];

    let api_ctx = convert_context(&ctx, true);
    assert!(
        api_ctx.shell.is_none(),
        "remote request must not include shell info"
    );
    assert!(
        api_ctx.operating_system.is_none(),
        "remote request must not include OS info"
    );
}

#[test]
fn convert_context_preserves_execution_environment_when_local() {
    let ctx = vec![AIAgentContext::ExecutionEnvironment(WarpAiExecutionContext {
        os: WarpAiOsContext {
            category: Some("darwin".to_string()),
            distribution: Some("macOS".to_string()),
        },
        shell_name: "zsh".to_string(),
        shell_version: Some("5.9".to_string()),
    })];

    let api_ctx = convert_context(&ctx, false);
    let shell = api_ctx.shell.expect("local request keeps shell info");
    assert_eq!(shell.name, "zsh");
    let os = api_ctx
        .operating_system
        .expect("local request keeps OS info");
    assert_eq!(os.platform, "darwin");
    assert_eq!(os.distribution, "macOS");
}

#[test]
fn convert_context_preserves_non_shell_context_for_remote() {
    // Directory + block context are routing-relevant (the endpoint still
    // needs a working directory and prior shell output), so they must
    // survive the remote-strip pass.
    let ctx = vec![
        AIAgentContext::Directory {
            pwd: Some("/Users/me/proj".to_string()),
            home_dir: Some("/Users/me".to_string()),
            are_file_symbols_indexed: false,
        },
        AIAgentContext::Block(Box::new(BlockContext {
            id: BlockId::default(),
            index: 0.into(),
            command: "ls -la".to_string(),
            output: "total 8".to_string(),
            exit_code: ExitCode::from(0),
            is_auto_attached: false,
            started_ts: None,
            finished_ts: None,
            pwd: None,
            shell: None,
            username: None,
            hostname: None,
            git_branch: None,
            os: None,
            session_id: None,
        })),
        AIAgentContext::ExecutionEnvironment(WarpAiExecutionContext {
            os: WarpAiOsContext {
                category: Some("darwin".to_string()),
                distribution: Some("macOS".to_string()),
            },
            shell_name: "zsh".to_string(),
            shell_version: Some("5.9".to_string()),
        }),
    ];

    let api_ctx = convert_context(&ctx, true);
    assert!(
        api_ctx.directory.is_some(),
        "directory context survives remote-strip"
    );
    #[allow(deprecated)]
    let executed_shell_count = api_ctx.executed_shell_commands.len();
    assert_eq!(
        executed_shell_count, 1,
        "block context (executed shell) survives remote-strip"
    );
    assert!(api_ctx.shell.is_none());
    assert!(api_ctx.operating_system.is_none());
}

#[test]
fn convert_context_isolates_two_simultaneous_endpoints() {
    // Two remote requests built independently must each be empty of local
    // execution info, regardless of which endpoint they target.
    let shell_ctx = vec![AIAgentContext::ExecutionEnvironment(
        WarpAiExecutionContext {
            os: WarpAiOsContext {
                category: Some("darwin".to_string()),
                distribution: Some("macOS".to_string()),
            },
            shell_name: "zsh".to_string(),
            shell_version: Some("5.9".to_string()),
        },
    )];

    let api_ctx_a = convert_context(&shell_ctx, true);
    let api_ctx_b = convert_context(&shell_ctx, true);
    assert!(api_ctx_a.shell.is_none());
    assert!(api_ctx_b.shell.is_none());
    assert_eq!(
        api_ctx_a.shell.is_none(),
        api_ctx_b.shell.is_none(),
        "two simultaneous remote contexts produce equivalent (empty) shell payloads"
    );
}
