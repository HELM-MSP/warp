use super::*;

#[test]
fn ask_user_question_skipped_by_auto_approve_converts_to_skipped_answers() {
    let result = api::request::input::tool_call_result::Result::from(
        AskUserQuestionResult::SkippedByAutoApprove {
            question_ids: vec!["q1".to_string(), "q2".to_string()],
        },
    );

    let api::request::input::tool_call_result::Result::AskUserQuestion(result) = result else {
        panic!("expected ask_user_question result");
    };

    let Some(api::ask_user_question_result::Result::Success(success)) = result.result else {
        panic!("expected success result");
    };

    assert_eq!(success.answers.len(), 2);
    assert_eq!(success.answers[0].question_id, "q1");
    assert_eq!(success.answers[1].question_id, "q2");
    assert!(matches!(
        success.answers[0].answer,
        Some(AskUserQuestionAnswer::Skipped(()))
    ));
    assert!(matches!(
        success.answers[1].answer,
        Some(AskUserQuestionAnswer::Skipped(()))
    ));
}

// hw-o8h: end-to-end conversion coverage for the four
// `LocalFallbackRefused` shell-side result variants. Before this fix,
// `WriteToLongRunningShellCommandResult`, `ReadShellCommandOutputResult`,
// and `TransferShellCommandControlToUserResult` all surfaced the refusal
// as `ConvertToAPITypeError::Ignore`, which aborts request construction
// outright. After the fix, they ride the proto's
// `ShellCommandError { command_not_found }` envelope so the server-side
// action loop sees a typed refusal.

#[test]
fn local_fallback_refused_write_converts_to_typed_error() {
    let result = api::request::input::tool_call_result::Result::try_from(
        WriteToLongRunningShellCommandResult::LocalFallbackRefused {
            reason: "session bound to remote endpoint".to_string(),
        },
    )
    .expect("refusal must serialize as typed tool error, not Ignore");
    match result {
        api::request::input::tool_call_result::Result::WriteToLongRunningShellCommand(
            api::WriteToLongRunningShellCommandResult { result: Some(inner) },
        ) => match inner {
            api::write_to_long_running_shell_command_result::Result::Error(
                api::ShellCommandError { r#type: Some(api::shell_command_error::Type::CommandNotFound(())) },
            ) => {}
            other => panic!("expected typed CommandNotFound shell error, got {other:?}"),
        },
        other => panic!("expected write tool call result, got {other:?}"),
    }
}

#[test]
fn local_fallback_refused_read_converts_to_typed_error() {
    let result = api::request::input::tool_call_result::Result::try_from(
        ReadShellCommandOutputResult::LocalFallbackRefused {
            reason: "session bound to remote endpoint".to_string(),
        },
    )
    .expect("refusal must serialize as typed tool error, not Ignore");
    match result {
        api::request::input::tool_call_result::Result::ReadShellCommandOutput(
            api::ReadShellCommandOutputResult {
                result: Some(inner),
                command,
            },
        ) => {
            assert_eq!(command, "", "refusal carries no command payload");
            match inner {
                api::read_shell_command_output_result::Result::Error(
                    api::ShellCommandError { r#type: Some(api::shell_command_error::Type::CommandNotFound(())) },
                ) => {}
                other => panic!("expected typed CommandNotFound shell error, got {other:?}"),
            }
        }
        other => panic!("expected read tool call result, got {other:?}"),
    }
}

#[test]
fn local_fallback_refused_transfer_converts_to_typed_error() {
    let result = api::request::input::tool_call_result::Result::try_from(
        TransferShellCommandControlToUserResult::LocalFallbackRefused {
            reason: "session bound to remote endpoint".to_string(),
        },
    )
    .expect("refusal must serialize as typed tool error, not Ignore");
    match result {
        api::request::input::tool_call_result::Result::TransferShellCommandControlToUser(
            api::TransferShellCommandControlToUserResult { result: Some(inner) },
        ) => match inner {
            api::transfer_shell_command_control_to_user_result::Result::Error(
                api::ShellCommandError { r#type: Some(api::shell_command_error::Type::CommandNotFound(())) },
            ) => {}
            other => panic!("expected typed CommandNotFound shell error, got {other:?}"),
        },
        other => panic!("expected transfer tool call result, got {other:?}"),
    }
}

#[test]
fn local_fallback_refused_request_command_converts_to_typed_denial() {
    // RequestCommandOutputResult's LocalFallbackRefused has been a typed
    // PermissionDenied since hw-c6z. Pin it down here alongside the new
    // write/read/transfer coverage so all four refusals have a single
    // end-to-end test surface.
    let result = api::request::input::tool_call_result::Result::try_from(
        RequestCommandOutputResult::LocalFallbackRefused {
            reason: "session bound to remote endpoint".to_string(),
        },
    )
    .expect("refusal must serialize as typed tool error, not Ignore");
    match result {
        #[allow(deprecated)]
        api::request::input::tool_call_result::Result::RunShellCommand(
            api::RunShellCommandResult {
                result: Some(inner),
                ..
            },
        ) => match inner {
            api::run_shell_command_result::Result::PermissionDenied(
                api::PermissionDenied {
                    reason: Some(api::permission_denied::Reason::DenylistedCommand(())),
                },
            ) => {}
            other => panic!("expected typed PermissionDenied/DenylistedCommand, got {other:?}"),
        },
        other => panic!("expected run-shell-command result, got {other:?}"),
    }
}

#[test]
fn local_fallback_refused_does_not_abort_request_construction() {
    // Regression guard: if any future change flips one of the four
    // variants back to `ConvertToAPITypeError::Ignore`, this test fires.
    // The whole point of hw-o8h is to *not* abort request construction.
    let variants: Vec<api::request::input::tool_call_result::Result> = vec![
        api::request::input::tool_call_result::Result::try_from(
            RequestCommandOutputResult::LocalFallbackRefused {
                reason: "r".to_string(),
            },
        )
        .unwrap(),
        api::request::input::tool_call_result::Result::try_from(
            WriteToLongRunningShellCommandResult::LocalFallbackRefused {
                reason: "r".to_string(),
            },
        )
        .unwrap(),
        api::request::input::tool_call_result::Result::try_from(
            ReadShellCommandOutputResult::LocalFallbackRefused {
                reason: "r".to_string(),
            },
        )
        .unwrap(),
        api::request::input::tool_call_result::Result::try_from(
            TransferShellCommandControlToUserResult::LocalFallbackRefused {
                reason: "r".to_string(),
            },
        )
        .unwrap(),
    ];
    assert_eq!(variants.len(), 4, "all four refusal variants must serialize");
}
