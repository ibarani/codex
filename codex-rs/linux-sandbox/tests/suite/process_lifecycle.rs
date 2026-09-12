//! Real namespace cleanup must close descendant-held output before fixture fallback.

#![cfg(target_os = "linux")]

use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

const PHASE_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);
const COMMAND_READY: &[u8] = b"command-ready\n";
const DESCENDANT_READY: &[u8] = b"descendant-ready\n";
const OUTPUT_LIMIT: u64 = 4096;

// Both shells inherit stdout. The descendant cannot close it successfully on
// its own: reaching the finite loop limit or the stop guard emits another byte.
// Removing the private directory also releases the fixture if a test unwinds.
const SCRIPT: &str = r#"
root=$1
[ -d "$root" ] && [ ! -e "$root/stop" ] || exit 80
(
    printf 'descendant-ready\n'
    ticks=0
    while [ "$ticks" -lt 600 ] && [ -d "$root" ] && [ ! -e "$root/stop" ]; do
        sleep 0.05
        ticks=$((ticks + 1))
    done
    printf 'fixture-stop\n'
) &
printf 'command-ready\n'
read -r action
[ "$action" = exit ] || exit 81
exit 0
"#;

#[derive(Clone, Copy, Debug)]
enum Finish {
    KillOuter,
    ExitInitialCommand,
}

#[tokio::test]
async fn killing_outer_sandbox_stops_ready_command_and_descendant() {
    assert_process_cleanup(Finish::KillOuter).await;
}

#[tokio::test]
async fn initial_command_exit_stops_its_running_descendant() {
    assert_process_cleanup(Finish::ExitInitialCommand).await;
}

async fn assert_process_cleanup(finish: Finish) {
    let fixture = tempfile::Builder::new()
        .prefix("sandbox-lifecycle-")
        .permissions(std::fs::Permissions::from_mode(/*mode*/ 0o700))
        .tempdir()
        .expect("create private lifecycle fixture");
    let root = fixture.path();
    let writable_root = AbsolutePathBuf::try_from(root).expect("absolute fixture directory");
    let profile = PermissionProfile::workspace_write_with(
        &[writable_root],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );
    let stderr_path = root.join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create fixture stderr");
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-linux-sandbox"));
    command
        .arg("--sandbox-policy-cwd")
        .arg(root)
        .arg("--permission-profile")
        .arg(serde_json::to_string(&profile).expect("serialize fixture permissions"))
        .args(["--", "/bin/sh", "-c", SCRIPT, "sandbox-lifecycle"])
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("TMPDIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .kill_on_drop(true);
    for key in ["RUNFILES_DIR", "RUNFILES_MANIFEST_FILE", "TEST_SRCDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("launch real Linux sandbox");
    let mut stdout = child.stdout.take().expect("capture sandbox stdout");
    let mut readiness = [0; COMMAND_READY.len() + DESCENDANT_READY.len()];
    let ready_read = timeout(PHASE_TIMEOUT, stdout.read_exact(&mut readiness)).await;
    let ready = matches!(ready_read, Ok(Ok(_)))
        && (readiness == [COMMAND_READY, DESCENDANT_READY].concat().as_slice()
            || readiness == [DESCENDANT_READY, COMMAND_READY].concat().as_slice());

    let action = if ready {
        match finish {
            Finish::KillOuter => child.start_kill(),
            Finish::ExitInitialCommand => {
                let stdin = child.stdin.as_mut().expect("retain command control pipe");
                match timeout(PHASE_TIMEOUT, stdin.write_all(b"exit\n")).await {
                    Ok(result) => result,
                    Err(error) => Err(std::io::Error::other(error)),
                }
            }
        }
    } else {
        Err(std::io::Error::other(
            "command and descendant did not become ready",
        ))
    };
    let native_status = timeout(PHASE_TIMEOUT, child.wait()).await;
    let mut extra = [0; 1];
    let native_eof = timeout(PHASE_TIMEOUT, stdout.read(&mut extra)).await;
    let closed_without_fallback = matches!(native_eof, Ok(Ok(0)));

    // Preserve the native observation before releasing anything ourselves.
    // A later EOF after this stop marker or stdin closure cannot pass the test.
    let stop_written = std::fs::write(root.join("stop"), b"stop");
    drop(child.stdin.take());
    if native_status.is_err() {
        let _ = child.start_kill();
    }
    let fallback_status = timeout(PHASE_TIMEOUT, child.wait()).await;
    let mut tail = Vec::new();
    let fallback_drain = timeout(
        PHASE_TIMEOUT,
        stdout.take(OUTPUT_LIMIT + 1).read_to_end(&mut tail),
    )
    .await;
    let mut stderr = Vec::new();
    std::fs::File::open(&stderr_path)
        .expect("open fixture stderr")
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut stderr)
        .expect("read bounded fixture stderr");
    let stderr = String::from_utf8_lossy(&stderr);

    assert!(
        stop_written.is_ok(),
        "fixture stop marker failed: {stop_written:?}"
    );
    assert!(
        matches!(fallback_status, Ok(Ok(_))) && matches!(fallback_drain, Ok(Ok(_))),
        "owned fixture failed to drain: status={fallback_status:?}, output={fallback_drain:?}; {stderr}"
    );
    assert!(tail.len() as u64 <= OUTPUT_LIMIT && stderr.len() as u64 <= OUTPUT_LIMIT);
    assert!(
        ready,
        "readiness failed: {ready_read:?}, bytes={readiness:?}; {stderr}"
    );
    assert!(
        action.is_ok(),
        "lifecycle action failed: {action:?}; {stderr}"
    );
    let status = native_status
        .expect("sandbox should exit before fixture fallback")
        .expect("reap sandbox after lifecycle action");
    match finish {
        Finish::KillOuter => assert_eq!(status.signal(), Some(libc::SIGKILL)),
        Finish::ExitInitialCommand => assert_eq!(status.code(), Some(0)),
    }
    assert!(
        closed_without_fallback,
        "descendant-held stdout remained open or emitted fixture fallback: {native_eof:?}; {stderr}"
    );
}
