//! Lifetime of the manager's three blocking plugin refresh workers.
//!
//! The app-server owns these workers independently of the strong manager Arcs
//! they retain. Admission shares the shutdown lock, cancellation reaches Git,
//! HTTP futures and curated lock waiting, and only completed handles are joined.
//! An incomplete drain remains visible; a non-returning filesystem operation or
//! other blocking system call is not claimed cancelled. Defaults, manual operation semantics and callbacks are
//! preserved. Cancellation prevents later operations, not rollback of writes
//! already completed. Direct manager users own explicit shutdown themselves.

use std::future::Future;
use std::io;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::watch;

#[derive(Clone, Default)]
pub(crate) struct PluginCancellation {
    receiver: Option<watch::Receiver<bool>>,
}

impl PluginCancellation {
    pub(crate) fn check(&self) -> Result<(), String> {
        if self
            .receiver
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow() || receiver.has_changed().is_err())
        {
            Err("plugin background task cancelled".to_string())
        } else {
            Ok(())
        }
    }

    pub(crate) async fn cancelled(&self) {
        match &self.receiver {
            Some(receiver) => {
                let mut receiver = receiver.clone();
                let _ = receiver.wait_for(|cancelled| *cancelled).await;
            }
            None => std::future::pending().await,
        }
    }

    pub(crate) async fn run<T>(
        &self,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::select! {
            biased;
            _ = self.cancelled() => Err("plugin background task cancelled".to_string()),
            result = operation => result,
        }
    }

    pub(crate) fn git(
        &self,
        command: Command,
        context: &str,
        timeout: Option<Duration>,
    ) -> Result<Output, String> {
        self.check()?;
        let run = move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to prepare {context}: {:?}", error.kind()))?;
            let mut command = tokio::process::Command::from(command);
            runtime
                .block_on(codex_git_utils::run_git_command_with_cancellation(
                    &mut command,
                    timeout,
                    self.cancelled(),
                ))
                .map_err(|error| format!("{context}: {:?}", error.kind()))
        };
        // Existing synchronous plugin APIs are also called from async request
        // handlers. A scoped thread preserves those blocking semantics without
        // nesting a runtime or detaching another worker.
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .name("plugin-git-command".to_string())
                    .spawn_scoped(scope, run)
                    .map_err(|error| format!("failed to start {context}: {:?}", error.kind()))?
                    .join()
                    .map_err(|_| "plugin Git command worker panicked".to_string())?
            })
        } else {
            run()
        }
    }
}

pub(crate) struct PluginBackgroundTasks {
    cancelled: watch::Sender<bool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for PluginBackgroundTasks {
    fn default() -> Self {
        Self {
            cancelled: watch::channel(/*init*/ false).0,
            threads: Mutex::new(Vec::new()),
        }
    }
}

impl PluginBackgroundTasks {
    pub(crate) fn spawn(
        &self,
        name: &str,
        task: impl FnOnce(PluginCancellation) + Send + 'static,
    ) -> io::Result<()> {
        let mut threads = self
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *self.cancelled.borrow() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "plugin background tasks are shut down",
            ));
        }
        reap_finished(&mut threads);
        // Spawn and registration share the admission lock with shutdown. A
        // worker cannot acquire an untracked lifetime after cancellation.
        let cancellation = PluginCancellation {
            receiver: Some(self.cancelled.subscribe()),
        };
        threads.push(
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || task(cancellation))?,
        );
        Ok(())
    }

    pub(crate) fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        {
            let _threads = self
                .threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.cancelled.send_replace(/*value*/ true);
        }
        loop {
            let mut threads = self
                .threads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reap_finished(&mut threads);
            if threads.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            drop(threads);
            std::thread::sleep(
                Duration::from_millis(/*millis*/ 10)
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn reap_finished(threads: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < threads.len() {
        if threads[index].is_finished() {
            let thread = threads.swap_remove(index);
            if thread.join().is_err() {
                tracing::warn!("plugin background worker panicked");
            }
        } else {
            index += 1;
        }
    }
}

#[cfg(test)]
#[path = "background_tasks_tests.rs"]
mod tests;
