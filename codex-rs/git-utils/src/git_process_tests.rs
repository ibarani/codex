use super::run_git_command_with_cancellation;
#[cfg(unix)]
use super::run_git_command_with_timeout_output;
#[cfg(unix)]
use pretty_assertions::assert_eq;
#[cfg(unix)]
use std::time::Duration;
use tokio::process::Command;

#[cfg(unix)]
#[tokio::test]
async fn completed_git_preserves_output_and_exit_status() -> anyhow::Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf output; printf diagnostic >&2; exit 7"]);
    let output = run_git_command_with_cancellation(
        &mut command,
        Some(Duration::from_secs(/*secs*/ 2)),
        std::future::pending(),
    )
    .await?;
    assert_eq!(output.stdout, b"output");
    assert_eq!(output.stderr, b"diagnostic");
    assert_eq!(output.status.code(), Some(7));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_git_is_not_spawned() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("spawned");
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "printf unexpected > \"$1\"", "cancelled-fixture"])
        .arg(&marker);
    let result = run_git_command_with_cancellation(
        &mut command,
        /*timeout_duration*/ None,
        std::future::ready(()),
    )
    .await;
    assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn git_spawn_failure_remains_an_error() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut command = Command::new(directory.path().join("absent-command"));
    assert!(
        run_git_command_with_cancellation(
            &mut command,
            /*timeout_duration*/ None,
            std::future::pending()
        )
        .await
        .is_err()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::git_process::run_spawned_git_command;
    use crate::git_process::spawn_git_command;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::process::Command;

    // Finite owned fixture. If cleanup fails, directory disappearance or the
    // stop file releases it; no saved numeric PID is ever signalled by a test.
    const SCRIPT: &str = r#"
(
    attempts=0
    while [ -d "$1" ] && [ ! -f "$1/stop" ] && [ "$attempts" -lt 1000 ]; do
        sleep 0.01
        attempts=$((attempts + 1))
    done
    printf survived > "$1/survived"
) &
child=$!
printf '%s\n' "$child" > "$1/child"
if [ "$2" = exit ]; then exit 0; fi
wait "$child"
"#;

    fn process_state(pid: u32) -> anyhow::Result<Option<(bool, u64)>> {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let tail = stat
                    .rsplit_once(')')
                    .ok_or_else(|| anyhow::anyhow!("invalid owned process stat"))?
                    .1;
                let fields = tail.split_whitespace().collect::<Vec<_>>();
                let live = !matches!(fields.first().copied(), Some("Z" | "X"));
                let started = fields
                    .get(19)
                    .ok_or_else(|| anyhow::anyhow!("missing owned process identity"))?
                    .parse()?;
                Ok(Some((live, started)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn live_process(pid: u32) -> anyhow::Result<bool> {
        Ok(process_state(pid)?.is_some_and(|(live, _)| live))
    }

    fn same_process_running(pid: u32, started: u64) -> anyhow::Result<bool> {
        Ok(process_state(pid)?.is_some_and(|(live, observed)| live && observed == started))
    }

    async fn wait_until(mut predicate: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(/*secs*/ 2);
        while !predicate()? {
            anyhow::ensure!(Instant::now() < deadline, "owned fixture deadline");
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
        Ok(())
    }

    async fn child_pid(directory: &Path) -> anyhow::Result<u32> {
        let mut pid = None;
        wait_until(|| {
            let record = match std::fs::read_to_string(directory.join("child")) {
                Ok(record) => record,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !record.ends_with('\n') {
                return Ok(false);
            }
            let observed: u32 = record.trim().parse()?;
            anyhow::ensure!(observed > 0, "invalid owned fixture PID");
            pid = Some(observed);
            Ok(true)
        })
        .await?;
        pid.ok_or_else(|| anyhow::anyhow!("owned fixture PID missing"))
    }

    async fn assert_cleanup(exited_root: bool, cancel: bool) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", SCRIPT, "git-tree-fixture"])
            .arg(directory.path())
            .arg(if exited_root { "exit" } else { "wait" });
        let process = spawn_git_command(&mut command)?;
        let root = process
            .child
            .id()
            .ok_or_else(|| anyhow::anyhow!("missing owned root"))?;
        let descendant = child_pid(directory.path()).await?;
        let descendant_started = process_state(descendant)?
            .ok_or_else(|| anyhow::anyhow!("fixture descendant missing before cleanup"))?
            .1;
        if exited_root {
            // Read-only observation of the retained Child; unlike try_wait,
            // this proves actual exit without relinquishing PID ownership.
            wait_until(|| live_process(root).map(|live| !live)).await?;
            assert_eq!(process.child.id(), Some(root));
        }
        let result = run_spawned_git_command(
            process,
            Some(Duration::from_millis(/*millis*/ 100)),
            async {
                if cancel {
                    return;
                }
                std::future::pending::<()>().await
            },
        )
        .await;
        let expected = if cancel {
            std::io::ErrorKind::Interrupted
        } else {
            std::io::ErrorKind::TimedOut
        };
        assert!(matches!(result, Err(error) if error.kind() == expected));
        // The PID came from this fixture before release. Observation alone can
        // never target an unrelated process; a reused live identity fails closed.
        let stopped =
            wait_until(|| same_process_running(descendant, descendant_started).map(|live| !live))
                .await;
        std::fs::write(directory.path().join("stop"), "stop")?;
        stopped?;
        assert!(!directory.path().join("survived").exists());
        Ok(())
    }

    #[tokio::test]
    async fn timed_out_git_wrapper_does_not_leave_child_process_running() -> anyhow::Result<()> {
        assert_cleanup(/*exited_root*/ false, /*cancel*/ false).await
    }

    #[tokio::test]
    async fn timed_out_exited_git_wrapper_does_not_leave_child_process_running()
    -> anyhow::Result<()> {
        assert_cleanup(/*exited_root*/ true, /*cancel*/ false).await
    }

    #[tokio::test]
    async fn cancelled_git_wrapper_terminates_tree() -> anyhow::Result<()> {
        assert_cleanup(/*exited_root*/ false, /*cancel*/ true).await
    }

    #[tokio::test]
    async fn cancelled_exited_git_wrapper_terminates_tree() -> anyhow::Result<()> {
        assert_cleanup(/*exited_root*/ true, /*cancel*/ true).await
    }

    #[tokio::test]
    async fn dropped_git_future_keeps_unreaped_tree_ownership() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", SCRIPT, "git-drop-fixture"])
            .arg(directory.path())
            .arg("exit");
        let process = spawn_git_command(&mut command)?;
        let root = process
            .child
            .id()
            .ok_or_else(|| anyhow::anyhow!("missing owned root"))?;
        let descendant = child_pid(directory.path()).await?;
        let descendant_started = process_state(descendant)?
            .ok_or_else(|| anyhow::anyhow!("fixture descendant missing before cleanup"))?
            .1;
        wait_until(|| live_process(root).map(|live| !live)).await?;
        let mut operation = Box::pin(run_spawned_git_command(
            process,
            /*timeout_duration*/ None,
            std::future::pending(),
        ));
        assert!(futures::poll!(operation.as_mut()).is_pending());
        drop(operation);
        let stopped =
            wait_until(|| same_process_running(descendant, descendant_started).map(|live| !live))
                .await;
        std::fs::write(directory.path().join("stop"), "stop")?;
        stopped?;
        Ok(())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_timeout_api_preserves_absence_result() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]);
    assert!(
        run_git_command_with_timeout_output(&mut command, Duration::from_millis(/*millis*/ 10))
            .await
            .is_none()
    );
}

// Preserve the original tree regression on the other supported platforms.
#[cfg(any(windows, all(unix, not(target_os = "linux"))))]
mod other_platforms {
    use crate::git_process::run_spawned_git_command;
    use crate::git_process::spawn_git_command;
    use pretty_assertions::assert_eq;
    #[cfg(windows)]
    use std::process::Stdio;
    use std::time::Duration;
    #[cfg(unix)]
    use tokio::io::AsyncReadExt as _;
    use tokio::process::Command;

    #[derive(Clone, Copy)]
    enum GitWrapperLifetime {
        WaitForChild,
        ExitBeforeTimeout,
    }

    async fn assert_timed_out_git_wrapper_does_not_leave_child_process_running(
        wrapper_lifetime: GitWrapperLifetime,
    ) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let child_pid_file = temp_dir.path().join("child.pid");
        let child_ready_file = temp_dir.path().join("child-ready");
        let release_child_file = temp_dir.path().join("release-child");
        let child_survived_file = temp_dir.path().join("child-survived");
        let release_wrapper_file = temp_dir.path().join("release-wrapper");
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            let wrapper_command = match wrapper_lifetime {
                GitWrapperLifetime::WaitForChild => {
                    r#"( : > "$CHILD_READY_FILE"; attempts=0; while [ -d "$FIXTURE_DIR" ] && [ ! -f "$RELEASE_CHILD_FILE" ] && [ "$attempts" -lt 1000 ]; do sleep 0.01; attempts=$((attempts + 1)); done; sleep 1; : > "$CHILD_SURVIVED_FILE" ) >/dev/null & child_pid=$!; printf '%s\n' "$child_pid" > "$CHILD_PID_FILE"; wait "$child_pid""#
                }
                GitWrapperLifetime::ExitBeforeTimeout => {
                    r#"( : > "$CHILD_READY_FILE"; attempts=0; while [ -d "$FIXTURE_DIR" ] && [ ! -f "$RELEASE_CHILD_FILE" ] && [ "$attempts" -lt 1000 ]; do sleep 0.01; attempts=$((attempts + 1)); done; sleep 1; : > "$CHILD_SURVIVED_FILE" ) >/dev/null & child_pid=$!; printf '%s\n' "$child_pid" > "$CHILD_PID_FILE"; while [ ! -f "$RELEASE_WRAPPER_FILE" ]; do sleep 0.01; done"#
                }
            };
            command.args(["-c", wrapper_command]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            let child_command = "Set-Content -LiteralPath $env:CHILD_READY_FILE -Value ready; while (-not (Test-Path $env:RELEASE_CHILD_FILE)) { Start-Sleep -Milliseconds 25 }; Start-Sleep -Seconds 1; Set-Content -LiteralPath $env:CHILD_SURVIVED_FILE -Value survived; Start-Sleep -Seconds 60";
            let wrapper_command = match wrapper_lifetime {
                GitWrapperLifetime::WaitForChild => format!(
                    "$child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', '{child_command}') -PassThru -NoNewWindow; [System.IO.File]::WriteAllText($env:CHILD_PID_FILE, [string]$child.Id); Wait-Process -Id $child.Id"
                ),
                GitWrapperLifetime::ExitBeforeTimeout => format!(
                    "$child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', '{child_command}') -PassThru -NoNewWindow; [System.IO.File]::WriteAllText($env:CHILD_PID_FILE, [string]$child.Id); while (-not (Test-Path $env:RELEASE_WRAPPER_FILE)) {{ Start-Sleep -Milliseconds 25 }}"
                ),
            };
            command
                .args(["-NoProfile", "-NonInteractive", "-Command"])
                .arg(wrapper_command);
            command
        };
        command
            .env("FIXTURE_DIR", temp_dir.path())
            .env("CHILD_PID_FILE", &child_pid_file)
            .env("CHILD_READY_FILE", &child_ready_file)
            .env("RELEASE_CHILD_FILE", &release_child_file)
            .env("CHILD_SURVIVED_FILE", &child_survived_file)
            .env("RELEASE_WRAPPER_FILE", &release_wrapper_file);

        let mut wrapper = spawn_git_command(&mut command).expect("spawn Git wrapper");
        let child_pid = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(child_pid) = std::fs::read_to_string(&child_pid_file)
                    && !child_pid.trim().is_empty()
                    && child_ready_file.exists()
                {
                    break child_pid.trim().to_string();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("wait for Git wrapper child readiness");

        if matches!(wrapper_lifetime, GitWrapperLifetime::ExitBeforeTimeout) {
            std::fs::write(&release_wrapper_file, "release").expect("release Git wrapper");
            #[cfg(unix)]
            {
                // Only the root owns stdout; its descendant retains stderr.
                // Observe root EOF without reaping the identity needed to stop
                // that descendant when the overall output deadline expires.
                let mut root_output = Vec::new();
                tokio::time::timeout(
                    Duration::from_secs(/*secs*/ 10),
                    wrapper
                        .child
                        .stdout
                        .as_mut()
                        .expect("root stdout")
                        .read_to_end(&mut root_output),
                )
                .await
                .expect("wait for Git wrapper EOF")
                .expect("read Git wrapper EOF");
                assert!(root_output.is_empty());
                assert!(wrapper.child.id().is_some());
            }
            #[cfg(windows)]
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if wrapper
                        .child
                        .try_wait()
                        .expect("check Git wrapper state")
                        .is_some()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("wait for Git wrapper exit");
        }

        let output = run_spawned_git_command(
            wrapper,
            Some(Duration::from_millis(/*millis*/ 100)),
            std::future::pending(),
        )
        .await
        .ok();
        assert_eq!(output, None);

        std::fs::write(&release_child_file, "release").expect("release Git wrapper child");
        tokio::time::sleep(Duration::from_secs(3)).await;
        if !child_survived_file.exists() {
            return;
        }

        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child_pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        panic!("Git wrapper child process {child_pid} survived timeout cleanup");
    }

    #[tokio::test]
    async fn timed_out_git_wrapper_does_not_leave_child_process_running() {
        assert_timed_out_git_wrapper_does_not_leave_child_process_running(
            GitWrapperLifetime::WaitForChild,
        )
        .await;
    }

    #[tokio::test]
    async fn timed_out_exited_git_wrapper_does_not_leave_child_process_running() {
        assert_timed_out_git_wrapper_does_not_leave_child_process_running(
            GitWrapperLifetime::ExitBeforeTimeout,
        )
        .await;
    }
}
