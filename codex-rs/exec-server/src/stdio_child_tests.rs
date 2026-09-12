use std::fs::File;
use std::io;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::runtime::Builder;

use super::StdioChild;
use super::StdioTransport;
use super::terminate_stdio_child;

// Both the leader and a descendant retain stdout and ignore TERM. A stop file,
// directory disappearance, or the finite loop lets a failing-original fixture
// finish without signalling any saved numeric PID. That path emits a marker and
// cannot satisfy the pre-fallback EOF oracle.
const SCRIPT: &str = r#"
trap '' TERM
(
    printf 'descendant-ready\n'
    attempts=0
    while [ -d "$1" ] && [ ! -f "$1/stop" ] && [ "$attempts" -lt 1000 ]; do
        sleep 0.01
        attempts=$((attempts + 1))
    done
    printf 'fixture-stopped\n'
) &
printf 'leader-ready\n'
wait
"#;

async fn ready_fixture() -> anyhow::Result<(TempDir, Child, File)> {
    let fixture = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(/*mode*/ 0o700))
        .tempdir()?;
    let mut child = Command::new("/bin/sh")
        .args(["-c", SCRIPT, "stdio-drop-fixture"])
        .arg(fixture.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(/*pgroup*/ 0)
        // Protect fixture setup failures. Root-only kill still cannot close the
        // descendant-held stdout, so it cannot satisfy either regression.
        .kill_on_drop(/*kill_on_drop*/ true)
        .spawn()?;
    let stdout = child.stdout.take().context("fixture stdout")?;
    let mut reader = BufReader::new(stdout);
    let readiness = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), async {
        let mut lines = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            anyhow::ensure!(
                reader.read_line(&mut line).await? > 0,
                "fixture exited before readiness"
            );
            lines.push(line);
        }
        lines.sort();
        anyhow::ensure!(
            lines == ["descendant-ready\n", "leader-ready\n"],
            "unexpected fixture readiness"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    readiness??;
    anyhow::ensure!(
        reader.buffer().is_empty(),
        "unexpected buffered fixture output"
    );
    let stdout = File::from(reader.into_inner().into_owned_fd()?);
    // Tokio's conversion restores blocking mode; the bounded synchronous oracle
    // needs nonblocking reads after its runtime has been destroyed.
    let flags = rustix::fs::fcntl_getfl(&stdout)?;
    rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok((fixture, child, stdout))
}

fn read_until_closed(reader: &mut File) -> io::Result<Option<Vec<u8>>> {
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 2);
    let mut output = Vec::new();
    let mut buffer = [0; 128];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(Some(output)),
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                if output.len() > 1024 {
                    return Err(io::Error::other("fixture output limit"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(/*millis*/ 10));
    }
}

fn assert_tree_closed_before_fallback(fixture: &TempDir, reader: &mut File) -> anyhow::Result<()> {
    let before_fallback = read_until_closed(reader);
    // Release only this fixture even if the original owner leaked its descendant.
    std::fs::write(fixture.path().join("stop"), "stop")?;
    let drained = read_until_closed(reader)?;
    anyhow::ensure!(
        drained.is_some(),
        "fixture did not close after its stop marker"
    );
    assert_eq!(before_fallback?, Some(Vec::new()));
    Ok(())
}

#[test]
fn runtime_drop_before_stdio_supervisor_poll_terminates_tree() -> anyhow::Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (fixture, child, mut stdout) = runtime.block_on(ready_fixture())?;
    let entered = runtime.enter();
    let transport = StdioTransport::spawn(child);
    drop(entered);
    // This current-thread runtime has not polled the newly spawned supervisor.
    drop(runtime);
    assert_tree_closed_before_fallback(&fixture, &mut stdout)?;
    drop(transport);
    Ok(())
}

#[test]
fn runtime_drop_during_stdio_termination_grace_terminates_tree() -> anyhow::Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (fixture, child, mut stdout) = runtime.block_on(ready_fixture())?;
    let mut termination = Box::pin(async move {
        let mut owned_child = StdioChild { child };
        let process_group_id = owned_child.child.id();
        terminate_stdio_child(&mut owned_child.child, process_group_id).await;
    });
    runtime.block_on(async {
        // The fixture ignores TERM, so this exact future is suspended in its
        // normal two-second grace, not merely queued for a later first poll.
        assert!(futures::poll!(termination.as_mut()).is_pending());
    });
    drop(runtime);
    drop(termination);
    assert_tree_closed_before_fallback(&fixture, &mut stdout)?;
    Ok(())
}
