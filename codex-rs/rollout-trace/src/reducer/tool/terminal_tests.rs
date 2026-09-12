use pretty_assertions::assert_eq;
use serde_json::json;

use crate::model::ExecutionStatus;
use crate::model::ExecutionWindow;
use crate::model::RolloutTrace;
use crate::model::TerminalModelObservation;
use crate::model::TerminalObservationSource;
use crate::model::TerminalOperation;
use crate::model::TerminalOperationKind;
use crate::model::TerminalRequest;
use crate::model::TerminalResult;
use crate::model::TerminalSession;
use crate::model::ToolCallKind;
use crate::model::ToolCallSummary;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawToolCallRequester;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::append_completed_inference;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::generic_summary;
use crate::reducer::test_support::message;
use crate::reducer::test_support::private_tempdir;
use crate::reducer::test_support::start_turn;
use crate::reducer::test_support::trace_context;
use crate::replay_bundle;
use crate::writer::TraceWriter;

#[test]
fn exec_tool_reduces_to_terminal_operation_and_session() -> anyhow::Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    append_inference_with_tool_call(&writer)?;

    let invocation_payload = writer.write_json_payload(
        RawPayloadKind::ToolInvocation,
        &json!({
            "tool_name": "exec_command",
            "tool_namespace": null,
            "payload": {
                "type": "function",
                "arguments": "{\"cmd\":\"cargo test\"}"
            }
        }),
    )?;
    let invocation_payload_id = invocation_payload.raw_payload_id.clone();
    let _tool_start = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-1".to_string(),
            model_visible_call_id: Some("call-1".to_string()),
            code_mode_runtime_tool_id: None,
            requester: crate::raw_event::RawToolCallRequester::Model,
            kind: ToolCallKind::ExecCommand,
            summary: generic_summary("exec_command"),
            invocation_payload: Some(invocation_payload),
        },
    )?;

    let runtime_start_payload = writer.write_json_payload(
        RawPayloadKind::ToolRuntimeEvent,
        &json!({
            "call_id": "tool-1",
            "turn_id": "turn-1",
            "command": ["cargo", "test"],
            "cwd": "/repo"
        }),
    )?;
    let runtime_start_payload_id = runtime_start_payload.raw_payload_id.clone();
    let runtime_start = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeStarted {
            tool_call_id: "tool-1".to_string(),
            runtime_payload: runtime_start_payload,
        },
    )?;

    let runtime_end_payload = writer.write_json_payload(
        RawPayloadKind::ToolRuntimeEvent,
        &json!({
            "call_id": "tool-1",
            "process_id": "pty-1",
            "turn_id": "turn-1",
            "command": ["cargo", "test"],
            "cwd": "/repo",
            "stdout": "ok\n",
            "stderr": "",
            "exit_code": 0,
            "formatted_output": "ok\n",
            "status": "completed"
        }),
    )?;
    let runtime_end_payload_id = runtime_end_payload.raw_payload_id.clone();
    let runtime_end = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeEnded {
            tool_call_id: "tool-1".to_string(),
            status: ExecutionStatus::Completed,
            runtime_payload: runtime_end_payload,
        },
    )?;

    let result_payload = writer.write_json_payload(
        RawPayloadKind::ToolResult,
        &json!({
            "type": "direct_response",
            "response_item": {
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "ok\n"
            }
        }),
    )?;
    let result_payload_id = result_payload.raw_payload_id.clone();
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallEnded {
            tool_call_id: "tool-1".to_string(),
            status: ExecutionStatus::Completed,
            result_payload: Some(result_payload),
        },
    )?;

    start_turn(&writer, "turn-2")?;
    append_followup_with_tool_output(&writer)?;

    let rollout = replay_bundle(temp.path())?;
    let operation_id = "terminal_operation:1".to_string();
    let output_item_id = rollout.inference_calls["inference-2"]
        .request_item_ids
        .last()
        .expect("tool output item")
        .clone();

    assert_eq!(
        rollout.tool_calls["tool-1"].terminal_operation_id,
        Some(operation_id.clone()),
    );
    assert_eq!(
        rollout.tool_calls["tool-1"].raw_invocation_payload_id,
        Some(invocation_payload_id),
    );
    assert_eq!(
        rollout.tool_calls["tool-1"].raw_result_payload_id,
        Some(result_payload_id),
    );
    assert_eq!(
        rollout.tool_calls["tool-1"].raw_runtime_payload_ids,
        vec![
            runtime_start_payload_id.clone(),
            runtime_end_payload_id.clone()
        ],
    );
    assert_eq!(
        rollout.tool_calls["tool-1"].summary,
        ToolCallSummary::Terminal {
            operation_id: operation_id.clone(),
        },
    );
    assert_eq!(
        rollout.terminal_operations[&operation_id],
        TerminalOperation {
            operation_id: operation_id.clone(),
            terminal_id: Some("pty-1".to_string()),
            tool_call_id: "tool-1".to_string(),
            kind: TerminalOperationKind::ExecCommand,
            execution: ExecutionWindow {
                started_at_unix_ms: runtime_start.wall_time_unix_ms,
                started_seq: runtime_start.seq,
                ended_at_unix_ms: Some(runtime_end.wall_time_unix_ms),
                ended_seq: Some(runtime_end.seq),
                status: ExecutionStatus::Completed,
            },
            request: TerminalRequest::ExecCommand {
                command: vec!["cargo".to_string(), "test".to_string()],
                display_command: "cargo test".to_string(),
                cwd: "/repo".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            result: Some(TerminalResult {
                exit_code: Some(0),
                stdout: "ok\n".to_string(),
                stderr: String::new(),
                formatted_output: Some("ok\n".to_string()),
                original_token_count: None,
                chunk_id: None,
            }),
            model_observations: vec![TerminalModelObservation {
                call_item_ids: rollout.inference_calls["inference-1"]
                    .response_item_ids
                    .clone(),
                output_item_ids: vec![output_item_id],
                source: TerminalObservationSource::DirectToolCall,
            }],
            raw_payload_ids: vec![runtime_start_payload_id, runtime_end_payload_id],
        },
    );
    assert_eq!(
        rollout.terminal_sessions["pty-1"],
        TerminalSession {
            terminal_id: "pty-1".to_string(),
            thread_id: "thread-root".to_string(),
            created_by_operation_id: operation_id.clone(),
            operation_ids: vec![operation_id],
            execution: ExecutionWindow {
                started_at_unix_ms: runtime_start.wall_time_unix_ms,
                started_seq: runtime_start.seq,
                ended_at_unix_ms: None,
                ended_seq: None,
                status: ExecutionStatus::Running,
            },
        },
    );

    Ok(())
}

#[test]
fn write_stdin_operation_reuses_existing_terminal_session() -> anyhow::Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;

    let startup_payload = writer.write_json_payload(
        RawPayloadKind::ToolRuntimeEvent,
        &json!({
            "call_id": "tool-start",
            "process_id": "pty-1",
            "turn_id": "turn-1",
            "command": ["bash"],
            "cwd": "/repo"
        }),
    )?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-start".to_string(),
            model_visible_call_id: None,
            code_mode_runtime_tool_id: None,
            requester: crate::raw_event::RawToolCallRequester::Model,
            kind: ToolCallKind::ExecCommand,
            summary: generic_summary("exec_command"),
            invocation_payload: None,
        },
    )?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeStarted {
            tool_call_id: "tool-start".to_string(),
            runtime_payload: startup_payload,
        },
    )?;

    let stdin_payload = writer.write_json_payload(
        RawPayloadKind::ToolRuntimeEvent,
        &json!({
            "call_id": "tool-stdin",
            "process_id": "pty-1",
            "turn_id": "turn-1",
            "command": ["bash"],
            "cwd": "/repo",
            "interaction_input": "echo hi\n"
        }),
    )?;
    let _stdin_start = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-stdin".to_string(),
            model_visible_call_id: None,
            code_mode_runtime_tool_id: None,
            requester: crate::raw_event::RawToolCallRequester::Model,
            kind: ToolCallKind::WriteStdin,
            summary: generic_summary("write_stdin"),
            invocation_payload: None,
        },
    )?;
    let stdin_runtime_start = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeStarted {
            tool_call_id: "tool-stdin".to_string(),
            runtime_payload: stdin_payload,
        },
    )?;

    let rollout = replay_bundle(temp.path())?;
    let startup_operation_id = "terminal_operation:1".to_string();
    let stdin_operation_id = "terminal_operation:2".to_string();

    assert_eq!(
        rollout.terminal_sessions["pty-1"].operation_ids,
        vec![startup_operation_id, stdin_operation_id.clone()],
    );
    assert_eq!(
        rollout.terminal_operations[&stdin_operation_id],
        TerminalOperation {
            operation_id: stdin_operation_id.clone(),
            terminal_id: Some("pty-1".to_string()),
            tool_call_id: "tool-stdin".to_string(),
            kind: TerminalOperationKind::WriteStdin,
            execution: ExecutionWindow {
                started_at_unix_ms: stdin_runtime_start.wall_time_unix_ms,
                started_seq: stdin_runtime_start.seq,
                ended_at_unix_ms: None,
                ended_seq: None,
                status: ExecutionStatus::Running,
            },
            request: TerminalRequest::WriteStdin {
                stdin: "echo hi\n".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            result: None,
            model_observations: Vec::new(),
            raw_payload_ids: vec!["raw_payload:2".to_string()],
        },
    );

    Ok(())
}

#[test]
fn dispatch_write_stdin_payload_reduces_to_terminal_operation() -> anyhow::Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;

    let request_payload = writer.write_json_payload(
        RawPayloadKind::ToolInvocation,
        &json!({
            "tool_name": "write_stdin",
            "tool_namespace": null,
            "payload": {
                "type": "function",
                "arguments": json!({
                    "session_id": 123,
                    "chars": "echo hi\n",
                    "yield_time_ms": 250,
                    "max_output_tokens": 2000
                }).to_string()
            }
        }),
    )?;
    let request_payload_id = request_payload.raw_payload_id.clone();
    let tool_start = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-stdin".to_string(),
            model_visible_call_id: Some("call-stdin".to_string()),
            code_mode_runtime_tool_id: None,
            requester: crate::raw_event::RawToolCallRequester::Model,
            kind: ToolCallKind::WriteStdin,
            summary: generic_summary("write_stdin"),
            invocation_payload: Some(request_payload),
        },
    )?;

    let response_payload = writer.write_json_payload(
        RawPayloadKind::ToolResult,
        &json!({
            "type": "direct_response",
            "response_item": {
                "type": "function_call_output",
                "call_id": "call-stdin",
                "output": "hi\n"
            }
        }),
    )?;
    let response_payload_id = response_payload.raw_payload_id.clone();
    let tool_end = writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallEnded {
            tool_call_id: "tool-stdin".to_string(),
            status: ExecutionStatus::Completed,
            result_payload: Some(response_payload),
        },
    )?;

    let rollout = replay_bundle(temp.path())?;
    let operation_id = "terminal_operation:1".to_string();

    assert_eq!(
        rollout.tool_calls["tool-stdin"].terminal_operation_id,
        Some(operation_id.clone()),
    );
    assert_eq!(
        rollout.tool_calls["tool-stdin"].summary,
        ToolCallSummary::Terminal {
            operation_id: operation_id.clone(),
        },
    );
    assert_eq!(
        rollout.terminal_operations[&operation_id],
        TerminalOperation {
            operation_id: operation_id.clone(),
            terminal_id: Some("123".to_string()),
            tool_call_id: "tool-stdin".to_string(),
            kind: TerminalOperationKind::WriteStdin,
            execution: ExecutionWindow {
                started_at_unix_ms: tool_start.wall_time_unix_ms,
                started_seq: tool_start.seq,
                ended_at_unix_ms: Some(tool_end.wall_time_unix_ms),
                ended_seq: Some(tool_end.seq),
                status: ExecutionStatus::Completed,
            },
            request: TerminalRequest::WriteStdin {
                stdin: "echo hi\n".to_string(),
                yield_time_ms: Some(250),
                max_output_tokens: Some(2000),
            },
            result: Some(TerminalResult {
                exit_code: None,
                stdout: "hi\n".to_string(),
                stderr: String::new(),
                formatted_output: Some("hi\n".to_string()),
                original_token_count: None,
                chunk_id: None,
            }),
            model_observations: Vec::new(),
            raw_payload_ids: vec![request_payload_id, response_payload_id],
        },
    );
    assert_eq!(
        rollout.terminal_sessions["123"],
        TerminalSession {
            terminal_id: "123".to_string(),
            thread_id: "thread-root".to_string(),
            created_by_operation_id: operation_id.clone(),
            operation_ids: vec![operation_id],
            execution: ExecutionWindow {
                started_at_unix_ms: tool_start.wall_time_unix_ms,
                started_seq: tool_start.seq,
                ended_at_unix_ms: None,
                ended_seq: None,
                status: ExecutionStatus::Running,
            },
        },
    );

    Ok(())
}

#[test]
fn code_mode_write_stdin_result_projects_structured_exec_fields() -> anyhow::Result<()> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;

    let request_payload = writer.write_json_payload(
        RawPayloadKind::ToolInvocation,
        &json!({
            "tool_name": "write_stdin",
            "tool_namespace": null,
            "payload": {
                "type": "function",
                "arguments": json!({
                    "session_id": 456,
                    "chars": "",
                    "yield_time_ms": 1000,
                    "max_output_tokens": 4000
                }).to_string()
            }
        }),
    )?;
    let response_payload = writer.write_json_payload(
        RawPayloadKind::ToolResult,
        &json!({
            "type": "code_mode_response",
            "value": {
                "chunk_id": "abc123",
                "wall_time_seconds": 1.25,
                "exit_code": 0,
                "original_token_count": 3,
                "output": "done\n"
            }
        }),
    )?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::CodeCellStarted {
            runtime_cell_id: "cell-1".to_string(),
            model_visible_call_id: "call-code".to_string(),
            source_js: "await tools.write_stdin({ chars: '' })".to_string(),
        },
    )?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-stdin".to_string(),
            model_visible_call_id: None,
            code_mode_runtime_tool_id: Some("runtime-tool-1".to_string()),
            requester: crate::raw_event::RawToolCallRequester::CodeCell {
                runtime_cell_id: "cell-1".to_string(),
            },
            kind: ToolCallKind::WriteStdin,
            summary: generic_summary("write_stdin"),
            invocation_payload: Some(request_payload),
        },
    )?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallEnded {
            tool_call_id: "tool-stdin".to_string(),
            status: ExecutionStatus::Completed,
            result_payload: Some(response_payload),
        },
    )?;

    let rollout = replay_bundle(temp.path())?;
    assert_eq!(
        rollout.terminal_operations["terminal_operation:1"].result,
        Some(TerminalResult {
            exit_code: Some(0),
            stdout: "done\n".to_string(),
            stderr: String::new(),
            formatted_output: Some("done\n".to_string()),
            original_token_count: Some(3),
            chunk_id: Some("abc123".to_string()),
        }),
    );

    Ok(())
}

#[test]
fn intercepted_exec_patch_retains_runtime_without_inventing_a_terminal() -> anyhow::Result<()> {
    for status in [
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Cancelled,
    ] {
        let (begin, end) = patch_runtime_payloads(&status);
        let rollout = replay_patch_runtime(
            ToolCallKind::ExecCommand,
            RawToolCallRequester::Model,
            begin,
            end,
            status.clone(),
        )?;
        assert!(rollout.terminal_operations.is_empty());
        assert!(rollout.terminal_sessions.is_empty());
        let tool = &rollout.tool_calls["tool-patch"];
        assert_eq!(
            (
                &tool.kind,
                &tool.execution.status,
                &tool.terminal_operation_id
            ),
            (&ToolCallKind::ExecCommand, &status, &None)
        );
        assert_eq!(tool.raw_runtime_payload_ids.len(), 2);
        assert!(tool.raw_invocation_payload_id.is_some());
        assert!(tool.raw_result_payload_id.is_some());
        assert_eq!(
            rollout.inference_calls["inference-patch"].tool_call_ids_started_by_response,
            vec!["tool-patch"]
        );
    }
    Ok(())
}

#[test]
fn direct_patch_runtime_stays_a_patch_without_terminal_rows() -> anyhow::Result<()> {
    let (begin, end) = patch_runtime_payloads(&ExecutionStatus::Completed);
    let rollout = replay_patch_runtime(
        ToolCallKind::ApplyPatch,
        RawToolCallRequester::Model,
        begin,
        end,
        ExecutionStatus::Completed,
    )?;
    assert!(rollout.terminal_operations.is_empty());
    assert!(rollout.terminal_sessions.is_empty());
    let tool = &rollout.tool_calls["tool-patch"];
    assert_eq!(tool.kind, ToolCallKind::ApplyPatch);
    assert_eq!(tool.execution.status, ExecutionStatus::Completed);
    assert_eq!(tool.raw_runtime_payload_ids.len(), 2);
    Ok(())
}

#[test]
fn code_mode_intercepted_patch_uses_canonical_dispatch_identity() -> anyhow::Result<()> {
    let (begin, end) = patch_runtime_payloads(&ExecutionStatus::Completed);
    let rollout = replay_patch_runtime(
        ToolCallKind::ExecCommand,
        RawToolCallRequester::CodeCell {
            runtime_cell_id: "runtime-cell".to_string(),
        },
        begin,
        end,
        ExecutionStatus::Completed,
    )?;
    let tool = &rollout.tool_calls["tool-patch"];
    assert_eq!(
        tool.code_mode_runtime_tool_id.as_deref(),
        Some("runtime-tool-alias")
    );
    assert_eq!(
        rollout.code_cells["code_cell:call-code"].nested_tool_call_ids,
        vec!["tool-patch"]
    );
    assert!(rollout.terminal_operations.is_empty());
    assert!(rollout.terminal_sessions.is_empty());
    Ok(())
}

#[test]
fn malformed_or_mismatched_patch_runtime_is_not_accepted() {
    for field in ["auto_approved", "changes", "call_id"] {
        let (mut begin, end) = patch_runtime_payloads(&ExecutionStatus::Completed);
        begin.as_object_mut().unwrap().remove(field);
        assert!(
            replay_patch_runtime(
                ToolCallKind::ApplyPatch,
                RawToolCallRequester::Model,
                begin,
                end,
                ExecutionStatus::Completed
            )
            .is_err(),
            "missing {field}"
        );
    }
    for (phase, field, value) in [
        ("begin", "call_id", json!("different-tool")),
        ("begin", "turn_id", json!("different-turn")),
        ("begin", "auto_approved", json!("true")),
        (
            "begin",
            "changes",
            json!({"src/lib.rs": {"type": "unknown"}}),
        ),
        ("begin", "command", json!(["cargo", "test"])),
        ("end", "call_id", json!("different-tool")),
        ("end", "turn_id", json!("different-turn")),
        ("end", "success", json!(false)),
        ("end", "status", json!("failed")),
        ("end", "command", json!(["cargo", "test"])),
    ] {
        let (mut begin, mut end) = patch_runtime_payloads(&ExecutionStatus::Completed);
        if phase == "begin" {
            begin[field] = value;
        } else {
            end[field] = value;
        }
        assert!(
            replay_patch_runtime(
                ToolCallKind::ApplyPatch,
                RawToolCallRequester::Model,
                begin,
                end,
                ExecutionStatus::Completed
            )
            .is_err(),
            "{phase} {field}"
        );
    }
}

#[test]
fn patch_begin_cannot_finish_with_exec_runtime_payload() {
    let (begin, _) = patch_runtime_payloads(&ExecutionStatus::Completed);
    let end = json!({"call_id":"tool-patch", "turn_id":"turn-1", "command":["true"],
        "stdout":"", "stderr":"", "exit_code":0, "formatted_output":""});
    assert!(
        replay_patch_runtime(
            ToolCallKind::ExecCommand,
            RawToolCallRequester::Model,
            begin,
            end,
            ExecutionStatus::Completed
        )
        .is_err()
    );
}

#[test]
fn exec_begin_cannot_finish_with_patch_runtime_payload() {
    let (_, end) = patch_runtime_payloads(&ExecutionStatus::Completed);
    let begin =
        json!({"call_id":"tool-patch", "turn_id":"turn-1", "command":["true"], "cwd":"/repo"});
    assert!(
        replay_patch_runtime(
            ToolCallKind::ExecCommand,
            RawToolCallRequester::Model,
            begin,
            end,
            ExecutionStatus::Completed
        )
        .is_err()
    );
}

fn patch_runtime_payloads(status: &ExecutionStatus) -> (serde_json::Value, serde_json::Value) {
    let patch_status = match status {
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::Cancelled => "declined",
        ExecutionStatus::Running | ExecutionStatus::Aborted => panic!("not a patch outcome"),
    };
    let changes = json!({"src/lib.rs": {"type":"update", "unified_diff":"@@ -1 +1 @@\n-old\n+new\n", "move_path":null}});
    (
        json!({"call_id":"tool-patch", "turn_id":"turn-1", "auto_approved":true, "changes":changes}),
        json!({"call_id":"tool-patch", "turn_id":"turn-1", "stdout":"patch result", "stderr":"",
        "success":*status == ExecutionStatus::Completed, "changes":changes, "status":patch_status}),
    )
}

fn replay_patch_runtime(
    kind: ToolCallKind,
    requester: RawToolCallRequester,
    begin: serde_json::Value,
    end: serde_json::Value,
    status: ExecutionStatus,
) -> anyhow::Result<RolloutTrace> {
    let temp = private_tempdir()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    let tool_name = if kind == ToolCallKind::ApplyPatch {
        "apply_patch"
    } else {
        "exec_command"
    };
    let code_mode = matches!(requester, RawToolCallRequester::CodeCell { .. });
    let source_js = "await tools.exec_command({cmd: 'apply_patch'})";
    let (model_visible_call_id, code_mode_runtime_tool_id, output) = match &requester {
        RawToolCallRequester::Model => (
            Some("call-patch".to_string()),
            None,
            json!({"type":"function_call", "name":tool_name, "arguments":"{}", "call_id":"call-patch"}),
        ),
        RawToolCallRequester::CodeCell { .. } => (
            None,
            Some("runtime-tool-alias".to_string()),
            json!({"type":"custom_tool_call", "name":"exec", "input":source_js, "call_id":"call-code"}),
        ),
    };
    append_completed_inference(
        &writer,
        "thread-root",
        "turn-1",
        "inference-patch",
        vec![],
        vec![output],
    )?;
    if let RawToolCallRequester::CodeCell { runtime_cell_id } = &requester {
        writer.append_with_context(
            trace_context("turn-1"),
            RawTraceEventPayload::CodeCellStarted {
                runtime_cell_id: runtime_cell_id.clone(),
                model_visible_call_id: "call-code".to_string(),
                source_js: source_js.to_string(),
            },
        )?;
    }
    let invocation = writer.write_json_payload(RawPayloadKind::ToolInvocation,
        &json!({"tool_name":tool_name, "tool_namespace":null, "payload":{"type":"function", "arguments":"{}"}}))?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-patch".to_string(),
            model_visible_call_id,
            code_mode_runtime_tool_id,
            requester,
            kind,
            summary: generic_summary(tool_name),
            invocation_payload: Some(invocation),
        },
    )?;
    let runtime_payload = writer.write_json_payload(RawPayloadKind::ToolRuntimeEvent, &begin)?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeStarted {
            tool_call_id: "tool-patch".to_string(),
            runtime_payload,
        },
    )?;
    let runtime_payload = writer.write_json_payload(RawPayloadKind::ToolRuntimeEvent, &end)?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeEnded {
            tool_call_id: "tool-patch".to_string(),
            status: status.clone(),
            runtime_payload,
        },
    )?;
    let result = if code_mode {
        json!({"type":"code_mode_response", "value":"patch result"})
    } else {
        json!({"type":"direct_response", "response_item":{"type":"function_call_output", "call_id":"call-patch", "output":"patch result"}})
    };
    let result_payload = writer.write_json_payload(RawPayloadKind::ToolResult, &result)?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallEnded {
            tool_call_id: "tool-patch".to_string(),
            status,
            result_payload: Some(result_payload),
        },
    )?;
    replay_bundle(temp.path())
}

fn append_inference_with_tool_call(writer: &TraceWriter) -> anyhow::Result<()> {
    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({
            "input": [message("user", "run tests")]
        }),
    )?;
    writer.append(RawTraceEventPayload::InferenceStarted {
        inference_call_id: "inference-1".to_string(),
        thread_id: "thread-root".to_string(),
        codex_turn_id: "turn-1".to_string(),
        model: "gpt-test".to_string(),
        provider_name: "test-provider".to_string(),
        request_payload: request,
    })?;

    let response = writer.write_json_payload(
        RawPayloadKind::InferenceResponse,
        &json!({
            "response_id": "resp-1",
            "output_items": [{
                "type": "function_call",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo test\"}",
                "call_id": "call-1"
            }]
        }),
    )?;
    writer.append(RawTraceEventPayload::InferenceCompleted {
        inference_call_id: "inference-1".to_string(),
        response_id: Some("resp-1".to_string()),
        upstream_request_id: None,
        response_payload: response,
    })?;
    Ok(())
}

fn append_followup_with_tool_output(writer: &TraceWriter) -> anyhow::Result<()> {
    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({
            "previous_response_id": "resp-1",
            "input": [{
                "type": "function_call_output",
                "call_id": "call-1",
                "output": "ok\n"
            }]
        }),
    )?;
    writer.append(RawTraceEventPayload::InferenceStarted {
        inference_call_id: "inference-2".to_string(),
        thread_id: "thread-root".to_string(),
        codex_turn_id: "turn-2".to_string(),
        model: "gpt-test".to_string(),
        provider_name: "test-provider".to_string(),
        request_payload: request,
    })?;
    Ok(())
}
