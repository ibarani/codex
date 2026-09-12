use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::ChildTerminator;
use super::ProcessHandle;
use super::ProcessSignal;

const PRIVATE_ERROR: &str = "synthetic-backend-detail-must-not-escape";

#[derive(Default)]
struct Calls {
    kills: AtomicUsize,
    signals: AtomicUsize,
}

struct FailingTerminator {
    calls: Arc<Calls>,
    kill_failures: usize,
    signal_failures: usize,
}

impl ChildTerminator for FailingTerminator {
    fn kill(&mut self) -> io::Result<()> {
        self.calls.kills.fetch_add(/*val*/ 1, Ordering::SeqCst);
        if self.kill_failures > 0 {
            self.kill_failures -= 1;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                PRIVATE_ERROR,
            ));
        }
        Ok(())
    }

    fn signal(&mut self, _signal: ProcessSignal) -> io::Result<()> {
        self.calls.signals.fetch_add(/*val*/ 1, Ordering::SeqCst);
        if self.signal_failures > 0 {
            self.signal_failures -= 1;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                PRIVATE_ERROR,
            ));
        }
        Ok(())
    }
}

// This tests the real handle and owned terminator without spawning a process,
// sending a signal, or relying on a Tokio runtime or helper-task scheduling.
fn mock_handle(kill_failures: usize, signal_failures: usize) -> (ProcessHandle, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let (writer_tx, _writer_rx) = mpsc::channel(/*buffer*/ 1);
    (
        ProcessHandle {
            writer_tx: Mutex::new(Some(writer_tx)),
            killer: Mutex::new(Some(Box::new(FailingTerminator {
                calls: Arc::clone(&calls),
                kill_failures,
                signal_failures,
            }))),
            reader_handle: Mutex::new(/*t*/ None),
            reader_abort_handles: Mutex::new(Vec::new()),
            writer_handle: Mutex::new(/*t*/ None),
            wait_handle: Mutex::new(/*t*/ None),
            exit_status: Arc::new(AtomicBool::new(/*v*/ false)),
            exit_code: Arc::new(Mutex::new(/*t*/ None)),
            _pty_handles: Mutex::new(/*t*/ None),
            resizer: Mutex::new(/*t*/ None),
        },
        calls,
    )
}

#[test]
fn failed_termination_preserves_owned_retry_and_safe_error() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 1, /*signal_failures*/ 0);
    let Err(error) = handle.request_terminate() else {
        panic!("termination should report the injected failure");
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "process termination request failed");
    assert!(!format!("{error:?}").contains(PRIVATE_ERROR));
    assert!(matches!(handle.killer.lock(), Ok(killer) if killer.is_some()));
    assert_eq!(calls.kills.load(Ordering::SeqCst), 1);
    assert!(!handle.has_exited());
    assert_eq!(handle.exit_code(), None);

    assert!(handle.request_terminate().is_ok());
    assert!(matches!(handle.killer.lock(), Ok(killer) if killer.is_none()));
    assert!(!handle.has_exited());
    assert_eq!(handle.exit_code(), None);
    assert!(handle.request_terminate().is_ok());
    drop(handle);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_termination_is_retried_by_later_destruction() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 1, /*signal_failures*/ 0);
    assert!(handle.request_terminate().is_err());
    drop(handle);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_best_effort_termination_retains_explicit_retry() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 1, /*signal_failures*/ 0);
    handle.terminate();
    assert!(matches!(handle.killer.lock(), Ok(killer) if killer.is_some()));
    assert_eq!(calls.kills.load(Ordering::SeqCst), 1);
    assert!(handle.request_terminate().is_ok());
    drop(handle);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 2);
}

#[test]
fn successful_termination_is_not_sent_twice() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 0, /*signal_failures*/ 0);
    assert!(handle.request_terminate().is_ok());
    assert!(handle.signal(ProcessSignal::Interrupt).is_ok());
    handle.terminate();
    drop(handle);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 1);
    assert_eq!(calls.signals.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_signal_preserves_termination_and_safe_error() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 0, /*signal_failures*/ 1);
    let Err(error) = handle.signal(ProcessSignal::Interrupt) else {
        panic!("signal should report the injected failure");
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "process signal request failed");
    assert!(!format!("{error:?}").contains(PRIVATE_ERROR));
    assert!(matches!(handle.killer.lock(), Ok(killer) if killer.is_some()));
    assert!(handle.request_terminate().is_ok());
    drop(handle);
    assert_eq!(calls.signals.load(Ordering::SeqCst), 1);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_signal_can_be_retried() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 0, /*signal_failures*/ 1);
    assert!(handle.signal(ProcessSignal::Interrupt).is_err());
    assert!(handle.signal(ProcessSignal::Interrupt).is_ok());
    drop(handle);
    assert_eq!(calls.signals.load(Ordering::SeqCst), 2);
    // Windows success means termination; Unix interrupts leave cleanup ownership.
    assert_eq!(
        calls.kills.load(Ordering::SeqCst),
        usize::from(!cfg!(windows))
    );
}

#[test]
fn poisoned_terminator_lock_reports_failure_without_invoking_backend() {
    let (handle, calls) = mock_handle(/*kill_failures*/ 0, /*signal_failures*/ 0);
    let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(_guard) = handle.killer.lock() else {
            panic!("mock terminator lock was already poisoned");
        };
        panic!("synthetic lock poison");
    }));
    assert!(poison.is_err());
    let Err(termination) = handle.request_terminate() else {
        panic!("termination should reject a poisoned lock");
    };
    let Err(signal) = handle.signal(ProcessSignal::Interrupt) else {
        panic!("signal should reject a poisoned lock");
    };
    for error in [termination, signal] {
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "process terminator lock is poisoned");
    }
    assert_eq!(calls.kills.load(Ordering::SeqCst), 0);
    assert_eq!(calls.signals.load(Ordering::SeqCst), 0);
    // Restore the test-owned mutex invariant explicitly before exercising cleanup.
    handle.killer.clear_poison();
    drop(handle);
    assert_eq!(calls.kills.load(Ordering::SeqCst), 1);
}
