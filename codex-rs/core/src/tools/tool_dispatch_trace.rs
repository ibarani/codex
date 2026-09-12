//! Adapter between core tool dispatch objects and rollout-trace events.
//!
//! `codex-rollout-trace` owns the event schema and writer behavior. This module
//! keeps the core-specific mapping from registry invocations/results out of the
//! registry control flow.

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::ToolDispatchInvocation;
use codex_rollout_trace::ToolDispatchPayload;
use codex_rollout_trace::ToolDispatchRequester;
use codex_rollout_trace::ToolDispatchResult;
use codex_rollout_trace::ToolDispatchTraceContext;

/// Pairs each admitted dispatch with one terminal, including dropped futures.
pub(crate) struct ToolDispatchTrace {
    context: Option<ToolDispatchTraceContext>,
}

impl ToolDispatchTrace {
    pub(crate) fn start(invocation: &ToolInvocation) -> Self {
        let context = invocation
            .session
            .services
            .rollout_thread_trace
            .start_tool_dispatch_trace(|| tool_dispatch_invocation(invocation));
        Self {
            context: Some(context),
        }
    }

    pub(crate) fn record_completed(
        mut self,
        invocation: &ToolInvocation,
        call_id: &str,
        payload: &ToolPayload,
        result: &dyn ToolOutput,
    ) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        if !context.is_enabled() {
            return;
        }

        let result_payload = tool_dispatch_result(invocation, call_id, payload, result);
        let status = if result.success_for_logging() {
            ExecutionStatus::Completed
        } else {
            ExecutionStatus::Failed
        };
        context.record_completed(status, result_payload);
        self.context = None;
    }

    pub(crate) fn record_failed(mut self, error: &FunctionCallError) {
        if let Some(context) = self.context.take() {
            context.record_failed(error);
        }
    }
}

impl Drop for ToolDispatchTrace {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            if std::thread::panicking() {
                context.record_failed("tool dispatch panicked before completion");
            } else {
                context.record_cancelled();
            }
        }
    }
}

fn tool_dispatch_invocation(invocation: &ToolInvocation) -> Option<ToolDispatchInvocation> {
    let requester = match &invocation.source {
        ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => {
            ToolDispatchRequester::Model {
                model_visible_call_id: invocation.call_id.clone(),
            }
        }
        ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => ToolDispatchRequester::CodeCell {
            runtime_cell_id: cell_id.clone(),
            runtime_tool_call_id: runtime_tool_call_id.clone(),
        },
    };

    Some(ToolDispatchInvocation {
        thread_id: invocation.session.thread_id.to_string(),
        codex_turn_id: invocation.turn.sub_id.clone(),
        tool_call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.name.clone(),
        tool_namespace: invocation
            .tool_name
            .namespace
            .as_ref()
            .filter(|_| !invocation.tool_name.is_default_namespace())
            .cloned(),
        requester,
        payload: tool_dispatch_payload(&invocation.payload),
    })
}

fn tool_dispatch_result(
    invocation: &ToolInvocation,
    call_id: &str,
    payload: &ToolPayload,
    result: &dyn ToolOutput,
) -> ToolDispatchResult {
    match invocation.source {
        ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => {
            ToolDispatchResult::DirectResponse {
                response_item: result.to_response_item(call_id, payload),
            }
        }
        ToolCallSource::CodeMode { .. } => ToolDispatchResult::CodeModeResponse {
            value: result.code_mode_result(payload),
        },
    }
}

fn tool_dispatch_payload(payload: &ToolPayload) -> ToolDispatchPayload {
    match payload {
        ToolPayload::Function { arguments } => ToolDispatchPayload::Function {
            arguments: arguments.clone(),
        },
        ToolPayload::ToolSearch { arguments } => ToolDispatchPayload::ToolSearch {
            arguments: arguments.clone(),
        },
        ToolPayload::Custom { input } => ToolDispatchPayload::Custom {
            input: input.clone(),
        },
    }
}

#[cfg(test)]
#[path = "tool_dispatch_trace_tests.rs"]
mod tests;
