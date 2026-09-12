use super::process::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;

pub(super) struct ControlledProcess {
    pub(super) process: Arc<UnifiedExecProcess>,
    pub(super) stdout: broadcast::Sender<Vec<u8>>,
    pub(super) exit: oneshot::Sender<i32>,
    pub(super) termination_requests: Arc<AtomicUsize>,
}

/// Drive the existing PTY adapter using finite channels without starting an OS process.
pub(super) async fn controlled_process() -> ControlledProcess {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel(1);
    let (stdout, stdout_rx) = broadcast::channel(8);
    let (exit, exit_rx) = oneshot::channel();
    let termination_requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = Arc::clone(&termination_requests);
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: Some(Box::new(move || {
            observed_requests.fetch_add(1, Ordering::SeqCst);
        })),
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(super::process::NoopSpawnLifecycle),
    )
    .await
    .expect("controlled process should start");
    ControlledProcess {
        process: Arc::new(process),
        stdout,
        exit,
        termination_requests,
    }
}

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

pub(super) async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            wake_tx,
        }),
        sandbox_type: Some(sandbox_type),
    };

    UnifiedExecProcess::from_exec_server_started(started)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(
        WriteStatus::UnknownProcess,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(
        WriteStatus::StdinClosed,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn remote_terminate_confirmed_rejects_request_failure_and_acknowledgment_without_exit() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("synthetic-private-transport-failure".to_string()),
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(
        !err.to_string()
            .contains("synthetic-private-transport-failure")
    );
    assert!(!process.has_exited());

    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let started = Instant::now();
    let err = process
        .terminate_confirmed()
        .await
        .expect_err("termination acknowledgment cannot stand in for an exit event");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    // Tokio rounds deadlines up to the next millisecond tick.
    assert!((Duration::from_secs(5)..=Duration::from_millis(5_001)).contains(&started.elapsed()));
    assert!(!process.has_exited());
    assert_eq!(process.exit_code(), None);
}

#[tokio::test(start_paused = true)]
async fn terminate_confirmed_waits_for_exit_and_preserves_trailing_output() {
    let ControlledProcess {
        process,
        stdout,
        exit,
        termination_requests,
    } = controlled_process().await;
    let mut termination = Box::pin(process.terminate_confirmed());
    assert!(futures::poll!(&mut termination).is_pending());
    assert_eq!(termination_requests.load(Ordering::SeqCst), 1);
    assert_eq!(process.exit_code(), None);
    exit.send(137).expect("deliver actual exit");
    tokio::task::yield_now().await;
    assert!(futures::poll!(&mut termination).is_pending());
    stdout
        .send(b"complete trailing output".to_vec())
        .expect("deliver output after exit");
    drop(stdout);
    termination
        .await
        .expect("exit and closed output confirm termination");
    assert_eq!(process.exit_code(), Some(137));
    assert_eq!(
        process
            .output_handles()
            .output_buffer
            .lock()
            .await
            .to_bytes(),
        b"complete trailing output"
    );
    process
        .terminate_confirmed()
        .await
        .expect("already observed termination is idempotent");
    assert_eq!(termination_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn terminate_confirmed_accepts_output_close_before_exit_but_not_close_alone() {
    let ControlledProcess {
        process,
        stdout,
        exit,
        ..
    } = controlled_process().await;
    drop(stdout);
    let mut termination = Box::pin(process.terminate_confirmed());
    assert!(futures::poll!(&mut termination).is_pending());
    tokio::task::yield_now().await;
    assert!(futures::poll!(&mut termination).is_pending());
    exit.send(143).expect("deliver exit after output close");
    termination
        .await
        .expect("observed exit after close confirms termination");

    let ControlledProcess {
        process,
        stdout,
        exit: _held_exit,
        ..
    } = controlled_process().await;
    drop(stdout);
    let started = Instant::now();
    process
        .terminate_confirmed()
        .await
        .expect_err("closed output does not establish exit");
    // Tokio rounds deadlines up to the next millisecond tick.
    assert!((Duration::from_secs(5)..=Duration::from_millis(5_001)).contains(&started.elapsed()));
    assert_eq!(process.exit_code(), None);
}

#[tokio::test(start_paused = true)]
async fn terminate_confirmed_rejects_failed_state_without_observed_exit() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;
    process.fail_and_terminate("synthetic disconnected transport".to_string());
    assert!(
        process.has_exited(),
        "legacy failed state alone is not an exit observation"
    );
    process
        .terminate_confirmed()
        .await
        .expect_err("transport failure must remain unconfirmed");
    assert_eq!(process.exit_code(), None);
}

#[tokio::test]
async fn remote_process_preserves_executor_sandbox_type() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::LinuxSeccomp,
    )
    .await;

    assert_eq!(
        process.sandbox_type(),
        codex_sandboxing::SandboxType::LinuxSeccomp
    );
}
