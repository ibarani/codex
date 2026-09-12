use std::future::Future;
use std::io;
#[cfg(unix)]
use std::io::Write as _;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;

use codex_protocol::shell_environment::scrub_non_inheritable_env_vars;
#[cfg(windows)]
use codex_utils_pty::JobObject;
#[cfg(unix)]
use codex_utils_pty::process_group::kill_process_group;
use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::process::Command;

struct GitChild {
    child: Child,
    #[cfg(windows)]
    job: Option<JobObject>,
}

impl Drop for GitChild {
    fn drop(&mut self) {
        // No child wait is polled while inherited output is still open. The
        // retained, unreaped child therefore owns this identity on cancellation.
        #[cfg(unix)]
        if let Some(process_id) = self.child.id()
            && let Err(error) = kill_process_group(process_id)
        {
            let _ = writeln!(
                io::stderr(),
                "failed to terminate cancelled Git process group: {:?}",
                error.kind()
            );
        }
        // Tokio's kill_on_drop retains the direct-child fallback; Windows keeps
        // the existing contained JobObject lifetime and root-only fallback.
    }
}

fn spawn_git_command(command: &mut Command) -> io::Result<GitChild> {
    scrub_non_inheritable_env_vars(command.as_std_mut());
    #[cfg(unix)]
    command.process_group(/*pgroup*/ 0);
    command.kill_on_drop(/*kill_on_drop*/ true);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    let (child, job) = match JobObject::create()
        .and_then(|job| job.spawn_contained(command).map(|child| (child, job)))
    {
        Ok((child, job)) => (child, Some(job)),
        Err(_) => {
            // A failed contained spawn leaves CREATE_SUSPENDED on the command.
            command.creation_flags(/*flags*/ 0);
            (command.spawn()?, None)
        }
    };
    #[cfg(not(windows))]
    let child = command.spawn()?;

    Ok(GitChild {
        child,
        #[cfg(windows)]
        job,
    })
}

async fn collect_git_output(process: &mut GitChild) -> io::Result<Output> {
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git stdout pipe was not available"))?;
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Git stderr pipe was not available"))?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    // Concurrent pipe reads avoid pipe-capacity deadlock. Reaping only after
    // both EOFs prevents a descendant-held pipe from leaving an armed stale PID.
    tokio::try_join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
    )?;
    let status = process.child.wait().await?;
    #[cfg(windows)]
    if let Some(job) = &process.job {
        job.preserve_descendants()?;
    }
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

/// Runs a Git command with an optional output/exit deadline and cancellation.
///
/// Cancellation, timeout, and I/O errors request process-tree termination through
/// the retained child owner. Successful completion preserves existing detached
/// descendants only after both output streams close. The cancellation future is
/// checked before spawning. Commands retain the existing Git environment policy.
pub async fn run_git_command_with_cancellation(
    command: &mut Command,
    timeout_duration: Option<Duration>,
    cancelled: impl Future<Output = ()>,
) -> io::Result<Output> {
    tokio::pin!(cancelled);
    tokio::select! {
        biased;
        _ = &mut cancelled => return Err(io::Error::new(io::ErrorKind::Interrupted, "Git operation cancelled")),
        _ = std::future::ready(()) => {},
    }
    let process = spawn_git_command(command)?;
    run_spawned_git_command(process, timeout_duration, cancelled).await
}

async fn run_spawned_git_command(
    mut process: GitChild,
    timeout_duration: Option<Duration>,
    cancelled: impl Future<Output = ()>,
) -> io::Result<Output> {
    tokio::pin!(cancelled);
    let result = tokio::select! {
        biased;
        _ = &mut cancelled => Err(io::Error::new(io::ErrorKind::Interrupted, "Git operation cancelled")),
        _ = async {
            match timeout_duration {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        } => Err(io::Error::new(io::ErrorKind::TimedOut, "Git operation timed out")),
        result = collect_git_output(&mut process) => result,
    };
    if result.is_err() && process.child.id().is_some() {
        #[cfg(unix)]
        if let Some(process_id) = process.child.id() {
            kill_process_group(process_id)?;
        }
        #[cfg(windows)]
        drop(process.job.take());
        process.child.start_kill()?;
        // Kill confirmation has a separate finite cleanup allowance; a failed
        // cleanup is reported, never converted into successful cancellation.
        tokio::time::timeout(Duration::from_secs(/*secs*/ 2), process.child.wait())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "Git cleanup did not confirm exit")
            })??;
    }
    result
}

pub(crate) async fn run_git_command_with_timeout_output(
    command: &mut Command,
    timeout_duration: Duration,
) -> Option<Output> {
    run_git_command_with_cancellation(command, Some(timeout_duration), std::future::pending())
        .await
        .ok()
}

#[cfg(test)]
#[path = "git_process_tests.rs"]
mod tests;
